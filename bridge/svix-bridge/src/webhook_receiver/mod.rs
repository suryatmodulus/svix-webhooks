use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{Path, State},
    routing::post,
    Router,
};
use svix_bridge_types::{
    async_trait,
    svix::api::{Svix, V1MessageStreamParams},
    ForwardRequest, PollerInput, ReceiverOutput, TransformationConfig, TransformerInput,
    TransformerInputFormat, TransformerJob, TransformerOutput, TransformerTx,
};
use tracing::instrument;
use types::{IntegrationId, IntegrationState, InternalState, SerializableRequest, Unvalidated};

use crate::{
    config::{PollerInputOpts, PollerReceiverConfig, WebhookReceiverConfig},
    webhook_receiver::types::SerializablePayload,
};

mod config;
mod types;
mod verification;

fn router() -> Router<InternalState, Body> {
    Router::new()
        .route(
            "/webhook/:integration_id",
            post(route).put(route).get(route).patch(route),
        )
        .route(
            "/webhook/:integration_id/",
            post(route).put(route).get(route).patch(route),
        )
}

pub async fn run(
    listen_addr: SocketAddr,
    routes: Vec<WebhookReceiverConfig>,
    transformer_tx: TransformerTx,
) -> std::io::Result<()> {
    let state = InternalState::from_receiver_configs(routes, transformer_tx)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let router = router().with_state(state);

    tracing::info!("Listening on: {listen_addr}");
    axum::Server::bind(&listen_addr)
        .serve(router.into_make_service())
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

#[instrument(
    skip_all,
    level = "error",
    fields(
        integration_id = integration_id.as_ref(),
    )
)]
async fn route(
    Path(integration_id): Path<IntegrationId>,
    State(InternalState {
        routes,
        transformer_tx,
    }): State<InternalState>,
    req: SerializableRequest<Unvalidated>,
) -> http::StatusCode {
    if let Some(IntegrationState {
        verifier,
        output,
        transformation,
    }) = routes.get(&integration_id)
    {
        match req.validate(verifier).await {
            Ok(req) => {
                let payload = match parse_payload(
                    req.payload(),
                    transformation.as_ref(),
                    transformer_tx.clone(),
                )
                .await
                {
                    Err(e) => return e,
                    Ok(p) => p,
                };
                match handle(payload, output.clone()).await {
                    Ok(value) => value,
                    Err(value) => return value,
                }
            }
            Err(code) => {
                tracing::warn!("validation failed: {code}");
                code
            }
        }
    } else {
        tracing::trace!("integration not found");
        http::StatusCode::NOT_FOUND
    }
}

// FIXME: Really odd return type - artifact of being extracted from the HTTP server
async fn handle(
    payload: ForwardRequest,
    output: Arc<Box<dyn ReceiverOutput>>,
) -> Result<http::StatusCode, http::StatusCode> {
    tracing::debug!("forwarding request");
    Ok(match output.handle(payload).await {
        Ok(_) => http::StatusCode::NO_CONTENT,
        Err(e) => {
            tracing::error!("Error forwarding request: {}", e);
            http::StatusCode::INTERNAL_SERVER_ERROR
        }
    })
}

/// Figures out how to build a JSON object from the payload, optionally running it through a
/// transformation.
///
/// WRT "raw" payloads, the return value here is going to be a JSON object regardless of whether
/// or not the queue producer wants "raw" data.
///
/// When there's no transformation defined we therefore attempt to parse the body as json.
/// When a transformation is defined, we branch to see if it expects string or json input.
///
/// For either case, we expect the value produced to match the schema of a [`ForwardRequest`].
async fn parse_payload(
    payload: &SerializablePayload,
    transformation: Option<&TransformationConfig>,
    transformer_tx: TransformerTx,
) -> Result<ForwardRequest, http::StatusCode> {
    match transformation {
        Some(xform) => {
            let input = match xform.format() {
                TransformerInputFormat::String => {
                    TransformerInput::String(payload.as_string().map_err(|_| {
                        tracing::error!("Unable to parse request body as string");
                        http::StatusCode::BAD_REQUEST
                    })?)
                }
                TransformerInputFormat::Json => {
                    TransformerInput::JSON(payload.as_json().map_err(|_| {
                        tracing::error!("Unable to parse request body as json");
                        http::StatusCode::BAD_REQUEST
                    })?)
                }
            };
            transform(input, xform.source().clone(), transformer_tx).await
        }
        // Keep the original payload as-is if there's no transformation specified, but stuff the
        // whole thing into the payload field.
        // The as_json() only gets us to `Value`, so we also need a `from_value` call to marshal
        // into a [`ForwardRequest`] type.
        None => Ok(ForwardRequest {
            payload: payload.as_json().map_err(|_| {
                tracing::error!("Unable to parse request body as json");
                http::StatusCode::BAD_REQUEST
            })?,
        }),
    }
}

/// Attempts to run the payload through a js transformation.
async fn transform(
    input: TransformerInput,
    script: String,
    tx: TransformerTx,
) -> Result<ForwardRequest, http::StatusCode> {
    let (job, callback) = TransformerJob::new(script, input);
    if let Err(e) = tx.send(job) {
        tracing::error!("transformations are not available: {}", e);
        return Err(http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    match callback.await {
        // This is the only "good" outcome giving a RHS value for the assignment.
        // All other match arms should bail with a non-2xx status.
        Ok(Ok(TransformerOutput::Object(obj))) => Ok(serde_json::from_value(
            serde_json::Value::Object(obj),
        )
        .map_err(|e| {
            tracing::error!("transformation produced invalid payload: {}", e);
            http::StatusCode::INTERNAL_SERVER_ERROR
        })?),
        Ok(Ok(TransformerOutput::Invalid)) => {
            tracing::error!("transformation produced invalid payload");
            Err(http::StatusCode::INTERNAL_SERVER_ERROR)
        }
        _ => {
            tracing::error!("transformation failed");
            Err(http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

struct SvixEventsPoller {
    name: String,
    input_opts: PollerInputOpts,
    transformation: Option<TransformationConfig>,
    transformer_tx: Option<TransformerTx>,
    svix_client: Svix,
    output: Arc<Box<dyn ReceiverOutput>>,
}

#[async_trait]
impl PollerInput for SvixEventsPoller {
    fn name(&self) -> &str {
        &self.name
    }

    fn set_transformer(&mut self, tx: Option<TransformerTx>) {
        self.transformer_tx = tx;
    }

    async fn run(&self) {
        run_inner(self).await
    }
}

impl PollerReceiverConfig {
    pub async fn into_poller_input(
        self,
        transformer_tx: TransformerTx,
    ) -> std::io::Result<Box<dyn PollerInput>> {
        let svix_client = self
            .input
            .svix_client()
            .expect("only one poller type; svix client required");
        let name = self.name.clone();
        let input_opts = self.input.clone();
        let transformation = self.transformation.clone();
        let output = Arc::new(
            self.into_receiver_output()
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        );
        Ok(Box::new(SvixEventsPoller {
            name,
            input_opts,
            transformation,
            transformer_tx: Some(transformer_tx.clone()),
            svix_client,
            output,
        }))
    }
}

async fn run_inner(poller: &SvixEventsPoller) -> ! {
    const MIN_SLEEP: Duration = Duration::from_millis(10);
    const MAX_SLEEP: Duration = Duration::from_secs(300);
    const NO_SLEEP: Duration = Duration::ZERO;
    let mut sleep_time = NO_SLEEP;

    let PollerInputOpts::SvixEvents {
        app_id,
        subscription_id,
        ..
    } = &poller.input_opts;

    let mut iterator = None;

    loop {
        tracing::trace!(app_id, subscription_id, "polling `/events`");
        match poller
            .svix_client
            .message()
            .events(V1MessageStreamParams {
                // FIXME: expose more params as poller input cfg
                app_id: app_id.clone(),
                limit: None,
                iterator: iterator.clone(),
                event_types: None,
                channels: None,
                after: None,
            })
            .await
        {
            Ok(resp) => {
                tracing::trace!(count = resp.data.len(), "got messages");
                for msg in resp.data.into_iter() {
                    let payload = match parse_payload(
                        &SerializablePayload::Standard(
                            // FIXME: for svix-event pollers we already know the payload is json so
                            //   there's some wasted ser/deser/ser cycles.
                            serde_json::to_vec(&msg)
                                .expect("just fetched as json, must be serializable"),
                        ),
                        poller.transformation.as_ref(),
                        poller
                            .transformer_tx
                            .clone()
                            .expect("transformer tx is required"),
                    )
                    .await
                    {
                        Err(status) => {
                            tracing::error!(
                                status = status.as_u16(),
                                "error while parsing polled message"
                            );
                            // BACKOFF
                            sleep_time = (sleep_time * 2).clamp(MIN_SLEEP, MAX_SLEEP);
                            // Retry the current iterator.
                            continue;
                        }
                        Ok(p) => p,
                    };
                    if let Err(status) = handle(payload, poller.output.clone()).await {
                        // FIXME: need to refactor handle to not give http status codes so we can report what happened here.
                        tracing::error!(
                            status = status.as_u16(),
                            "error while handling polled message"
                        );
                        // BACKOFF
                        sleep_time = (sleep_time * 2).clamp(MIN_SLEEP, MAX_SLEEP);
                    }
                }
                tracing::trace!(
                    iterator =? &iterator,
                    next_iterator =? &resp.iterator,
                    "batch handled, updating local iterator"
                );
                // Update the iterator _after we've handled all the messages in the batch_.
                iterator = Some(resp.iterator.clone());
                // If the iterator is "done" we can backoff to wait for new messages to arrive.
                sleep_time = if resp.done {
                    // BACKOFF
                    (sleep_time * 2).clamp(MIN_SLEEP, MAX_SLEEP)
                } else {
                    NO_SLEEP
                };
            }

            Err(err) => {
                tracing::trace!(
                    error =? err,
                    iterator =? &iterator,
                    "request failed, retrying current iterator"
                );
                // BACKOFF
                sleep_time = (sleep_time * 2).clamp(MIN_SLEEP, MAX_SLEEP);
            }
        }

        if !sleep_time.is_zero() {
            tracing::trace!(sleep_time=?sleep_time, "sleeping");
            tokio::time::sleep(sleep_time).await;
        }
    }
}

#[cfg(test)]
mod tests;
