// SPDX-FileCopyrightText: © 2022 Svix Authors
// SPDX-License-Identifier: MIT

use crate::cfg::Configuration;

use crate::core::cache::Cache;
use crate::core::cryptography::Encryption;
use crate::core::operational_webhooks::EndpointDisabledEvent;
use crate::core::types::{
    ApplicationId, ApplicationUid, EndpointId, EndpointSecretInternal, EndpointSecretType,
    MessageUid, OrganizationId,
};
use crate::core::{
    kv_backend::{kv_def, Key, KvBackend, Value},
    message_app::{CreateMessageApp, CreateMessageEndpoint},
    operational_webhooks::{MessageAttemptEvent, OperationalWebhook, OperationalWebhookSender},
    shared_store::SharedStore,
    types::{
        BaseId, EndpointHeaders, MessageAttemptId, MessageAttemptTriggerType, MessageId,
        MessageStatus,
    },
};
use crate::db::models::{endpoint, message, messageattempt, messagedestination};
use crate::error::{Error, Result};
use crate::queue::{
    MessageTask, MessageTaskBatch, QueueTask, TaskQueueConsumer, TaskQueueProducer,
};
use chrono::Utc;

use futures::future;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderName};
use sea_orm::{entity::prelude::*, ActiveValue::Set, DatabaseConnection, EntityTrait};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, Duration};

use std::{
    iter,
    str::FromStr,
    sync::{atomic::Ordering, Arc},
};

// The maximum variation from the retry schedule when applying jitter to a resent webhook event in
// percent deviation
const JITTER_DELTA: f32 = 0.2;

const USER_AGENT: &str = concat!("Svix-Webhooks/", env!("CARGO_PKG_VERSION"));
/// Send the MessageAttemptFailingEvent after exceeding this number of failed attempts
const OP_WEBHOOKS_SEND_FAILING_EVENT_AFTER: usize = 4;

/// A simple struct noting the context of the wrapped [`DateTimeUtc`]. This struct is returned when
/// you are to disable disable an endpoint. This is optionally returned by [`process_failure_store`]
/// which is to be called after all retry events are exhausted.
#[repr(transparent)]
struct EndpointDisableInfo {
    first_failure_at: DateTimeUtc,
}

/// The first_failure_at time is only stored in Postgres after the endpoint has been disabled.
/// Otherwise, it is stored in the store with an expiration.
#[derive(Deserialize, Serialize)]
pub struct FailureValue {
    pub first_failure_at: DateTimeUtc,
}

kv_def!(FailureKey, FailureValue, "SVIX_FAILURE_CACHE");

impl FailureKey {
    pub fn new(app_id: &ApplicationId, endp_id: &EndpointId) -> FailureKey {
        FailureKey(format!("{}_{}_{}", Self::KEY_PREFIX, app_id, endp_id))
    }
}

/// Called upon the successful dispatch of an endpoint. Simply clears the store of a
/// [`FailureCacheKey`]/[`FailureCacheValue`] pair associated with a given endpoint. This is such
/// that an endpoint that was previously not responding is not disabled after responding again.
///
/// If the key value pair does not already exist in the store, indicating that the endpoint never
/// stopped responding, no operation is performed.
async fn process_success_store(
    store: &SharedStore,
    app_id: &ApplicationId,
    endp_id: &EndpointId,
) -> Result<()> {
    let key = FailureKey::new(app_id, endp_id);

    store
        .delete(&key)
        .await
        .map_err(|e| Error::from(e.to_string()))?;

    Ok(())
}

/// Called upon endpoint failure. Returns whether to disable the endpoint based on the time of first
/// failure stored in the store.
///
/// If no failure has previously been reported, then now is stored as the time of first failure and
/// the endpoint is not disabled.
///
/// If there has been a  preivous failure, then it is compared to the configured grace period, where
/// if there have been only failures within the grace period, then the endpoint is disabled.
///
/// All store values are set with an expiration time greater thah the grace period, so occasional
/// failures will not cause an endpoint to be disabled.
async fn process_failure_store(
    store: &SharedStore,

    app_id: &ApplicationId,
    endp_id: &EndpointId,

    disable_in: Duration,
) -> Result<Option<EndpointDisableInfo>> {
    let key = FailureKey::new(app_id, endp_id);
    let now = Utc::now();

    // If it already exists in the store, see if the grace preiod has already elapsed
    if let Some(FailureValue { first_failure_at }) = store
        .get::<FailureValue>(&key)
        .await
        .map_err(|e| Error::from(e.to_string()))?
    {
        if now - first_failure_at
            > chrono::Duration::from_std(disable_in).expect("Given `disable_in` is too large")
        {
            Ok(Some(EndpointDisableInfo { first_failure_at }))
        } else {
            Ok(None)
        }
    }
    // If it does not yet exist in the store, set the first_failure_at value to now
    else {
        store
            .set(
                &key,
                &FailureValue {
                    first_failure_at: now,
                },
                // Failures are forgiven after double the `disable_in` `Duration` with the expiry of
                // the Redis key
                disable_in * 2,
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        Ok(None)
    }
}

/// Sign a message
fn sign_msg(
    main_secret: &Encryption,
    timestamp: i64,
    body: &str,
    msg_id: &MessageId,
    endpoint_signing_keys: &[&EndpointSecretInternal],
) -> String {
    let to_sign = format!("{}.{}.{}", msg_id, timestamp, body);
    endpoint_signing_keys
        .iter()
        .map(|x| {
            let sig = x.sign(main_secret, to_sign.as_bytes());
            let version = match x.type_() {
                EndpointSecretType::Hmac256 => "v1",
                EndpointSecretType::Ed25519 => "v1a",
            };
            format!("{},{}", version, base64::encode(sig))
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// Generates a set of headers for any one webhook event
fn generate_msg_headers(
    timestamp: i64,
    msg_id: &MessageId,
    signatures: String,
    whitelabel_headers: bool,
    configured_headers: Option<&EndpointHeaders>,
    _endpoint_url: &str,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let id = msg_id.0.parse().expect("Error parsing message id");
    let timestamp = timestamp
        .to_string()
        .parse()
        .expect("Error parsing message timestamp");
    let signatures_str = signatures
        .parse()
        .expect("Error parsing message signatures");
    if whitelabel_headers {
        headers.insert("webhook-id", id);
        headers.insert("webhook-timestamp", timestamp);
        headers.insert("webhook-signature", signatures_str);
    } else {
        headers.insert("svix-id", id);
        headers.insert("svix-timestamp", timestamp);
        headers.insert("svix-signature", signatures_str);
    }

    if let Some(configured_headers) = configured_headers {
        for (k, v) in &configured_headers.0 {
            if let (Ok(k), Ok(v)) = (HeaderName::from_str(k), v.parse()) {
                headers.insert(k, v);
            } else {
                tracing::error!("Invalid HeaderName or HeaderValues for `{}: {}`", k, v);
            }
        }
    }

    headers
}

#[derive(Clone)]
struct WorkerContext<'a> {
    cache: &'a Cache,
    cfg: &'a Configuration,
    db: &'a DatabaseConnection,
    store: &'a SharedStore,
    queue_tx: &'a TaskQueueProducer,
    op_webhook_sender: &'a OperationalWebhookSender,
}

struct DispatchExtraIds<'a> {
    org_id: &'a OrganizationId,
    app_uid: Option<&'a ApplicationUid>,
    msg_uid: Option<&'a MessageUid>,
}

/// Dispatches one webhook
#[tracing::instrument(
    skip_all,
    fields(
        org_id = org_id.0.as_str(),
        endp_id = msg_task.endpoint_id.0.as_str(),
        msg_id = msg_task.msg_id.0.as_str()
    )
)]
async fn dispatch(
    WorkerContext {
        store,
        cfg,
        db,
        queue_tx,
        op_webhook_sender,
        ..
    }: WorkerContext<'_>,
    msg_task: MessageTask,
    DispatchExtraIds {
        org_id,
        app_uid,
        msg_uid,
    }: DispatchExtraIds<'_>,
    payload: &Json,
    endp: CreateMessageEndpoint,
) -> Result<()> {
    tracing::trace!("Dispatch: {} {}", &msg_task.msg_id, &endp.id);

    let now = Utc::now();
    let body = serde_json::to_string(&payload).expect("Error parsing message body");
    let headers = {
        let keys: Vec<&EndpointSecretInternal> = if let Some(ref old_keys) = endp.old_signing_keys {
            iter::once(&endp.key)
                .chain(old_keys.0.iter().map(|x| &x.key))
                .collect()
        } else {
            vec![&endp.key]
        };

        let signatures = sign_msg(
            &cfg.encryption,
            now.timestamp(),
            &body,
            &msg_task.msg_id,
            &keys,
        );

        let mut headers = generate_msg_headers(
            now.timestamp(),
            &msg_task.msg_id,
            signatures,
            cfg.whitelabel_headers,
            endp.headers.as_ref(),
            &endp.url,
        );
        headers.insert("user-agent", USER_AGENT.to_string().parse().unwrap());
        headers
    };

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("Invalid reqwest Client configuration");
    let res = client
        .post(&endp.url)
        .headers(headers)
        .timeout(Duration::from_secs(cfg.worker_request_timeout as u64))
        .json(&payload)
        .send()
        .await;

    let msg_dest = messagedestination::Entity::secure_find_by_msg(msg_task.msg_id.clone())
        .filter(messagedestination::Column::EndpId.eq(endp.id.clone()))
        .one(db)
        .await?
        .ok_or_else(|| {
            Error::Generic(format!(
                "Msg dest not found {} {}",
                msg_task.msg_id, endp.id
            ))
        })?;

    if (msg_dest.status != MessageStatus::Pending && msg_dest.status != MessageStatus::Sending)
        && (msg_task.trigger_type != MessageAttemptTriggerType::Manual)
    {
        // TODO: it happens when this message destination is "resent". This leads to 2 queue tasks with the same message destination
        tracing::warn!(
            "MessageDestination {} is not pending (it's {:?}).",
            msg_dest.id,
            msg_dest.status
        );
        return Ok(());
    }

    let attempt = messageattempt::ActiveModel {
        // Set both ID and created_at to the same timestamp
        id: Set(MessageAttemptId::new(now.into(), None)),
        created_at: Set(now.into()),
        msg_id: Set(msg_task.msg_id.clone()),
        endp_id: Set(endp.id.clone()),
        msg_dest_id: Set(msg_dest.id.clone()),
        url: Set(endp.url.clone()),
        ended_at: Set(Some(Utc::now().into())),
        trigger_type: Set(msg_task.trigger_type),
        ..Default::default()
    };
    let attempt = match res {
        Ok(res) => {
            let status_code = res.status().as_u16() as i16;
            let status = if res.status().is_success() {
                MessageStatus::Success
            } else {
                MessageStatus::Fail
            };
            let http_error = res.error_for_status_ref().err();

            let bytes = res
                .bytes()
                .await
                .expect("Could not read endpoint response body");
            let body = bytes_to_string(bytes);

            let attempt = messageattempt::ActiveModel {
                response_status_code: Set(status_code),
                response: Set(body),
                status: Set(status),
                ..attempt
            };
            match http_error {
                Some(err) => Err((attempt, err)),
                None => Ok(attempt),
            }
        }
        Err(err) => {
            let attempt = messageattempt::ActiveModel {
                response_status_code: Set(0),
                response: Set(err.to_string()),
                status: Set(MessageStatus::Fail),

                ..attempt
            };
            Err((attempt, err))
        }
    };

    match attempt {
        Ok(attempt) => {
            let _attempt = attempt.insert(db).await?;

            let msg_dest = messagedestination::ActiveModel {
                status: Set(MessageStatus::Success),
                next_attempt: Set(None),
                ..msg_dest.into()
            };
            let msg_dest = msg_dest.update(db).await?;

            process_success_store(store, &msg_task.app_id, &msg_task.endpoint_id).await?;

            tracing::trace!("Worker success: {} {}", &msg_dest.id, &endp.id,);
        }
        Err((attempt, err)) => {
            let attempt = attempt.insert(db).await?;

            let attempt_count = msg_task.attempt_count as usize;
            if msg_task.trigger_type == MessageAttemptTriggerType::Manual {
                tracing::debug!("Manual retry failed");
            } else if attempt_count < cfg.retry_schedule.len() {
                tracing::debug!(
                    "Worker failure retrying for attempt {}: {} {} {}",
                    attempt_count,
                    err,
                    &msg_dest.id,
                    &endp.id
                );

                let duration = cfg.retry_schedule[attempt_count];

                // Apply jitter with a maximum variation of JITTER_DELTA
                let duration = rand::thread_rng().gen_range(
                    duration.mul_f32(1.0 - JITTER_DELTA)..duration.mul_f32(1.0 + JITTER_DELTA),
                );

                let msg_dest = messagedestination::ActiveModel {
                    next_attempt: Set(Some(
                        (Utc::now()
                            + chrono::Duration::from_std(duration)
                                .expect("Error parsing duration"))
                        .into(),
                    )),
                    ..msg_dest.into()
                };
                let _msg_dest = msg_dest.update(db).await?;

                if attempt_count == OP_WEBHOOKS_SEND_FAILING_EVENT_AFTER {
                    op_webhook_sender
                        .send_operational_webhook(
                            org_id,
                            OperationalWebhook::MessageAttemptFailing(MessageAttemptEvent {
                                app_id: &msg_task.app_id,
                                app_uid,
                                endpoint_id: &msg_task.endpoint_id,
                                msg_id: &msg_task.msg_id,
                                msg_event_id: msg_uid,
                                last_attempt: (&attempt).into(),
                            }),
                        )
                        .await?;
                }

                queue_tx
                    .send(
                        QueueTask::MessageV1(MessageTask {
                            attempt_count: msg_task.attempt_count + 1,
                            ..msg_task
                        }),
                        Some(duration),
                    )
                    .await?;
            } else {
                tracing::debug!(
                    "Worker failure attempts exhausted: {} {} {}",
                    err,
                    &msg_dest.id,
                    &endp.id
                );
                let msg_dest = messagedestination::ActiveModel {
                    status: Set(MessageStatus::Fail),
                    next_attempt: Set(None),
                    ..msg_dest.into()
                };
                let _msg_dest = msg_dest.update(db).await?;

                // Send common operational webhook
                op_webhook_sender
                    .send_operational_webhook(
                        org_id,
                        OperationalWebhook::MessageAttemptExhausted(MessageAttemptEvent {
                            app_id: &msg_task.app_id,
                            app_uid,
                            endpoint_id: &msg_task.endpoint_id,
                            msg_id: &msg_task.msg_id,
                            msg_event_id: msg_uid,
                            last_attempt: (&attempt).into(),
                        }),
                    )
                    .await?;

                match process_failure_store(
                    store,
                    &msg_task.app_id,
                    &msg_task.endpoint_id,
                    cfg.endpoint_failure_disable_after,
                )
                .await?
                {
                    None => {}

                    Some(EndpointDisableInfo { first_failure_at }) => {
                        // Send operational webhooks
                        op_webhook_sender
                            .send_operational_webhook(
                                org_id,
                                OperationalWebhook::EndpointDisabled(EndpointDisabledEvent {
                                    app_id: &msg_task.app_id,
                                    app_uid,
                                    endpoint_id: &msg_task.endpoint_id,
                                    // TODO:
                                    endpoint_uid: None,
                                    fail_since: first_failure_at,
                                }),
                            )
                            .await?;

                        // Disable endpoint in DB
                        let endp = endpoint::Entity::secure_find_by_id(
                            msg_task.app_id.clone(),
                            msg_task.endpoint_id.clone(),
                        )
                        .one(db)
                        .await?
                        .ok_or_else(|| {
                            Error::Generic(format!(
                                "Endpoint not found {} {}",
                                &msg_task.app_id, &msg_task.endpoint_id
                            ))
                        })?;

                        let endp = endpoint::ActiveModel {
                            disabled: Set(true),
                            first_failure_at: Set(Some(first_failure_at.into())),
                            ..endp.into()
                        };
                        let _endp = endp.update(db).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn bytes_to_string(bytes: bytes::Bytes) -> String {
    match std::str::from_utf8(&bytes) {
        Ok(v) => v.to_owned(),
        Err(_) => base64::encode(&bytes),
    }
}

/// Manages preparation and execution of a QueueTask type
#[tracing::instrument(skip_all)]
async fn process_task(worker_context: WorkerContext<'_>, queue_task: Arc<QueueTask>) -> Result<()> {
    let WorkerContext { cache, db, .. }: WorkerContext<'_> = worker_context;

    if *queue_task == QueueTask::HealthCheck {
        return Ok(());
    }

    let (msg_id, trigger_type) = match &*queue_task {
        QueueTask::MessageBatch(MessageTaskBatch {
            msg_id,
            trigger_type,
            ..
        }) => (msg_id, trigger_type),
        QueueTask::MessageV1(MessageTask {
            msg_id,
            trigger_type,
            ..
        }) => (msg_id, trigger_type),

        QueueTask::HealthCheck => unreachable!(),
    };

    let msg = message::Entity::find_by_id(msg_id.clone())
        .one(db)
        .await?
        .ok_or_else(|| Error::Generic(format!("Unexpected: message doesn't exist {}", msg_id,)))?;
    let payload = msg.payload.as_ref().expect("Message payload is NULL");

    let create_message_app = CreateMessageApp::layered_fetch(
        cache.clone(),
        db,
        None,
        msg.app_id.clone(),
        msg.org_id.clone(),
        Duration::from_secs(30),
    )
    .await?
    .ok_or_else(|| Error::Generic(format!("Application doesn't exist: {}", &msg.app_id)))?;

    let app_uid = create_message_app.uid.clone();

    let endpoints: Vec<CreateMessageEndpoint> = create_message_app
        .filtered_endpoints(*trigger_type, &msg)
        .iter()
        .filter(|endpoint| match &*queue_task {
            QueueTask::HealthCheck => unreachable!(),
            QueueTask::MessageV1(task) => task.endpoint_id == endpoint.id,
            QueueTask::MessageBatch(_) => true,
        })
        .cloned()
        .collect();

    // TODO: remove this section once destinations are obsolete
    if matches!(*queue_task, QueueTask::MessageBatch(_)) {
        let destinations = endpoints
            .iter()
            .map(|endpoint| messagedestination::ActiveModel {
                msg_id: Set(msg.id.clone()),
                endp_id: Set(endpoint.id.clone()),
                next_attempt: Set(Some(Utc::now().into())),
                status: Set(MessageStatus::Sending),
                ..Default::default()
            });
        messagedestination::Entity::insert_many(destinations)
            .exec(db)
            .await?;
    }

    let org_id = &msg.org_id;
    let msg_uid = &msg.uid;
    let futures: Vec<_> = endpoints
        .into_iter()
        .map(|endpoint| {
            let task = match &*queue_task {
                QueueTask::MessageV1(task) => task.clone(),
                QueueTask::MessageBatch(MessageTaskBatch {
                    msg_id,
                    app_id,
                    trigger_type,
                    ..
                }) => MessageTask {
                    msg_id: msg_id.clone(),
                    app_id: app_id.clone(),
                    endpoint_id: endpoint.id.clone(),
                    attempt_count: 0,
                    trigger_type: *trigger_type,
                },

                QueueTask::HealthCheck => unreachable!(),
            };

            dispatch(
                worker_context.clone(),
                task,
                DispatchExtraIds {
                    org_id,
                    app_uid: app_uid.as_ref(),
                    msg_uid: msg_uid.as_ref(),
                },
                payload,
                endpoint,
            )
        })
        .collect();

    let join = future::join_all(futures).await;

    let errs: Vec<_> = join.iter().filter(|x| x.is_err()).collect();
    if !errs.is_empty() {
        return Err(Error::Generic(format!(
            "Some dispatches failed unexpectedly: {:?}",
            errs
        )));
    }

    Ok(())
}

/// Listens on the message queue for new tasks
pub async fn worker_loop(
    cache: Cache,
    cfg: &Configuration,
    pool: &DatabaseConnection,
    store: SharedStore,
    queue_tx: TaskQueueProducer,
    mut queue_rx: TaskQueueConsumer,
    op_webhook_sender: OperationalWebhookSender,
) -> Result<()> {
    loop {
        match queue_rx.receive_all().await {
            Ok(batch) => {
                if crate::SHUTTING_DOWN.load(Ordering::SeqCst) {
                    for delivery in batch {
                        queue_tx.nack(delivery).await.expect(
                            "Error sending 'nack' to Redis after receiving shutdown signal",
                        );
                    }
                    break;
                }

                for delivery in batch {
                    let cache = cache.clone();
                    let cfg = cfg.clone();
                    let pool = pool.clone();
                    let store = store.clone();
                    let queue_tx = queue_tx.clone();
                    let queue_task = delivery.task.clone();
                    let op_webhook_sender = op_webhook_sender.clone();

                    tokio::spawn(async move {
                        let worker_context = WorkerContext {
                            cache: &cache,
                            cfg: &cfg,
                            db: &pool,
                            store: &store,
                            queue_tx: &queue_tx,
                            op_webhook_sender: &op_webhook_sender,
                        };

                        if let Err(err) = process_task(worker_context, queue_task).await {
                            tracing::error!("Error executing task: {}", err);
                            queue_tx
                                .nack(delivery)
                                .await
                                .expect("Error sending 'nack' to Redis after task execution error");
                        } else {
                            queue_tx.ack(delivery).await.expect(
                                "Error sending 'ack' to Redis after successful task execution",
                            );
                        }
                    });
                }
            }
            Err(err) => {
                tracing::error!("Error receiving task: {}", err);
                sleep(Duration::from_millis(10)).await;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cryptography::AsymmetricKey;
    use crate::core::types::{BaseId, EndpointSecret};

    use bytes::Bytes;
    use ed25519_compact::Signature;
    use std::collections::HashMap;

    // [`generate_msg_headers`] tests
    const TIMESTAMP: i64 = 1;
    const WHITELABEL_HEADERS: bool = false;
    const BODY: &str = "{\"test\": \"body\"}";
    const ENDPOINT_SIGNING_KEYS: &[&EndpointSecretInternal] = &[];
    const ENDPOINT_URL: &str = "http://localhost:8071";

    /// Utility function that returns the default set of headers before configurable header are
    /// accounted for
    fn mock_headers() -> (HeaderMap, MessageId) {
        let id = MessageId::new(None, None);

        let signatures = sign_msg(
            &Encryption::new_noop(),
            TIMESTAMP,
            BODY,
            &id,
            ENDPOINT_SIGNING_KEYS,
        );

        (
            generate_msg_headers(
                TIMESTAMP,
                &id,
                signatures,
                WHITELABEL_HEADERS,
                None,
                ENDPOINT_URL,
            ),
            id,
        )
    }

    // Tests configurable headers with a valid and an invalid header. The valid header pair should
    // be included, while the invalid pair should be skipped.
    #[test]
    fn test_generate_msg_headers_with_custom_headers() {
        // The headers to be given to [`generate_msg_headers`]
        let mut headers = HashMap::new();
        headers.insert("test_key".to_owned(), "value".to_owned());
        headers.insert("invälid_key".to_owned(), "value".to_owned());

        // The invalid key should be skipped over so it is not included in the expected
        let (mut expected, id) = mock_headers();
        let _ = expected.insert("test_key", "value".parse().unwrap());

        let signatures = sign_msg(
            &Encryption::new_noop(),
            TIMESTAMP,
            BODY,
            &id,
            ENDPOINT_SIGNING_KEYS,
        );

        let actual = generate_msg_headers(
            TIMESTAMP,
            &id,
            signatures,
            WHITELABEL_HEADERS,
            Some(&EndpointHeaders(headers)),
            ENDPOINT_URL,
        );

        assert_eq!(expected, actual);
    }

    // Tests endpoint signing keys -- expected values are fetched from the Svix documentation for a
    // direct comparison to the current implementation.
    #[test]
    fn test_generate_msg_headers_with_signing_key() {
        let test_timestamp = 1614265330;
        let test_body = "{\"test\": 2432232314}";
        let test_key = EndpointSecretInternal::from_endpoint_secret(
            EndpointSecret::Symmetric(base64::decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap()),
            &Encryption::new_noop(),
        )
        .unwrap();
        let test_message_id = MessageId("msg_p5jXN8AQM9LWM0D4loKWxJek".to_owned());

        let expected_signature_str = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";

        let signatures = sign_msg(
            &Encryption::new_noop(),
            test_timestamp,
            test_body,
            &test_message_id,
            &[&test_key],
        );

        let actual = generate_msg_headers(
            test_timestamp,
            &test_message_id,
            signatures,
            WHITELABEL_HEADERS,
            None,
            ENDPOINT_URL,
        );

        assert_eq!(
            actual.get("svix-signature").unwrap(),
            expected_signature_str
        );
    }

    // Tests asemmtric signing keys
    #[test]
    fn test_asymmetric_key_signing() {
        let timestamp = 1614265330;
        let body = "{\"test\": 2432232314}";
        let asym_key = AsymmetricKey::from_base64("6Xb/dCcHpPea21PS1N9VY/NZW723CEc77N4rJCubMbfVKIDij2HKpMKkioLlX0dRqSKJp4AJ6p9lMicMFs6Kvg==").unwrap();
        let test_key = EndpointSecretInternal::from_endpoint_secret(
            EndpointSecret::Asymmetric(asym_key.clone()),
            &Encryption::new_noop(),
        )
        .unwrap();
        let msg_id = MessageId("msg_p5jXN8AQM9LWM0D4loKWxJek".to_owned());

        let signatures = sign_msg(
            &Encryption::new_noop(),
            timestamp,
            body,
            &msg_id,
            &[&test_key],
        );

        let to_sign = format!("{}.{}.{}", msg_id, timestamp, body);
        assert!(signatures.starts_with("v1a,"));
        let sig: Signature = Signature::from_slice(
            base64::decode(&signatures["v1a,".len()..])
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        asym_key.0.pk.verify(to_sign.as_bytes(), &sig).unwrap();
        assert_eq!(signatures, "v1a,hnO3f9T8Ytu9HwrXslvumlUpqtNVqkhqw/enGzPCXe5BdqzCInXqYXFymVJaA7AZdpXwVLPo3mNl8EM+m7TBAg==");
    }

    #[test]
    fn test_bytes_to_string() {
        let b = Bytes::from_static(b"Hello, world.");
        assert_eq!(bytes_to_string(b), "Hello, world.");
    }
}
