mod cluster;

use std::time::Duration;

use bb8::{Pool, RunError};
use bb8_redis::RedisConnectionManager;
use redis::{FromRedisValue, RedisError, RedisResult};

pub use self::cluster::RedisClusterConnectionManager;
use crate::cfg::Configuration;

pub const REDIS_CONN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub enum RedisPool {
    Clustered(ClusteredRedisPool),
    ClusteredUnpooled(ClusteredRedisUnpooled),
    NonClustered(NonClusteredRedisPool),
}

impl RedisPool {
    pub async fn get(&self) -> Result<PooledConnection<'_>, RunError<RedisError>> {
        match self {
            Self::Clustered(pool) => pool.get().await,
            Self::NonClustered(pool) => pool.get().await,
            Self::ClusteredUnpooled(pool) => pool.get().await,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClusteredRedisPool {
    pool: Pool<RedisClusterConnectionManager>,
}

impl ClusteredRedisPool {
    pub async fn get(&self) -> Result<PooledConnection<'_>, RunError<RedisError>> {
        let con = ClusteredPooledConnection {
            con: self.pool.get().await?,
        };
        Ok(PooledConnection::Clustered(con))
    }
}

#[derive(Clone)]
pub struct ClusteredRedisUnpooled {
    con: redis::cluster_async::ClusterConnection,
}

impl ClusteredRedisUnpooled {
    pub async fn get(&self) -> Result<PooledConnection<'_>, RunError<RedisError>> {
        Ok(PooledConnection::ClusteredUnpooled(
            ClusteredUnpooledConnection {
                con: self.con.clone(),
            },
        ))
    }
}

impl std::fmt::Debug for ClusteredRedisUnpooled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusteredRedisUnpooled").finish()
    }
}

#[derive(Clone, Debug)]
pub struct NonClusteredRedisPool {
    pool: Pool<RedisConnectionManager>,
}

impl NonClusteredRedisPool {
    pub async fn get(&self) -> Result<PooledConnection<'_>, RunError<RedisError>> {
        let con = self.pool.get().await?;
        let con = NonClusteredPooledConnection { con };
        Ok(PooledConnection::NonClustered(con))
    }
}

pub enum PooledConnection<'a> {
    Clustered(ClusteredPooledConnection<'a>),
    ClusteredUnpooled(ClusteredUnpooledConnection),
    NonClustered(NonClusteredPooledConnection<'a>),
}

impl PooledConnection<'_> {
    pub async fn query_async<T: FromRedisValue>(&mut self, cmd: redis::Cmd) -> RedisResult<T> {
        cmd.query_async(self).await
    }

    pub async fn query_async_pipeline<T: FromRedisValue>(
        &mut self,
        pipe: redis::Pipeline,
    ) -> RedisResult<T> {
        pipe.query_async(self).await
    }
}

impl redis::aio::ConnectionLike for PooledConnection<'_> {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        match self {
            PooledConnection::Clustered(conn) => conn.con.req_packed_command(cmd),
            PooledConnection::NonClustered(conn) => conn.con.req_packed_command(cmd),
            PooledConnection::ClusteredUnpooled(conn) => conn.con.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a redis::Pipeline,
        offset: usize,
        count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        match self {
            PooledConnection::Clustered(conn) => conn.con.req_packed_commands(cmd, offset, count),
            PooledConnection::NonClustered(conn) => {
                conn.con.req_packed_commands(cmd, offset, count)
            }
            PooledConnection::ClusteredUnpooled(conn) => {
                conn.con.req_packed_commands(cmd, offset, count)
            }
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            PooledConnection::Clustered(conn) => conn.con.get_db(),
            PooledConnection::NonClustered(conn) => conn.con.get_db(),
            PooledConnection::ClusteredUnpooled(conn) => conn.con.get_db(),
        }
    }
}

pub struct NonClusteredPooledConnection<'a> {
    con: bb8::PooledConnection<'a, RedisConnectionManager>,
}

impl<'a> NonClusteredPooledConnection<'a> {
    pub async fn query_async<T: FromRedisValue>(&mut self, cmd: redis::Cmd) -> RedisResult<T> {
        cmd.query_async(&mut *self.con).await
    }

    pub async fn query_async_pipeline<T: FromRedisValue>(
        &mut self,
        pipe: redis::Pipeline,
    ) -> RedisResult<T> {
        pipe.query_async(&mut *self.con).await
    }
}

pub struct ClusteredPooledConnection<'a> {
    con: bb8::PooledConnection<'a, RedisClusterConnectionManager>,
}

impl<'a> ClusteredPooledConnection<'a> {
    pub async fn query_async<T: FromRedisValue>(&mut self, cmd: redis::Cmd) -> RedisResult<T> {
        cmd.query_async(&mut *self.con).await
    }

    pub async fn query_async_pipeline<T: FromRedisValue>(
        &mut self,
        pipe: redis::Pipeline,
    ) -> RedisResult<T> {
        pipe.query_async(&mut *self.con).await
    }
}

pub struct ClusteredUnpooledConnection {
    con: redis::cluster_async::ClusterConnection,
}

impl ClusteredUnpooledConnection {
    pub async fn query_async<T: FromRedisValue>(&mut self, cmd: redis::Cmd) -> RedisResult<T> {
        cmd.query_async(&mut self.con).await
    }

    pub async fn query_async_pipeline<T: FromRedisValue>(
        &mut self,
        pipe: redis::Pipeline,
    ) -> RedisResult<T> {
        pipe.query_async(&mut self.con).await
    }
}

async fn new_redis_pool_helper(
    redis_dsn: &str,
    clustered: bool,
    max_connections: u16,
) -> RedisPool {
    if clustered {
        let mgr = RedisClusterConnectionManager::new(redis_dsn)
            .expect("Error initializing redis cluster client");
        let pool = bb8::Pool::builder()
            .max_size(max_connections.into())
            .build(mgr)
            .await
            .expect("Error initializing redis cluster connection pool");
        let pool = ClusteredRedisPool { pool };
        RedisPool::Clustered(pool)
    } else {
        let mgr = RedisConnectionManager::new(redis_dsn).expect("Error initializing redis client");
        let pool = bb8::Pool::builder()
            .max_size(max_connections.into())
            .build(mgr)
            .await
            .expect("Error initializing redis connection pool");
        let pool = NonClusteredRedisPool { pool };
        RedisPool::NonClustered(pool)
    }
}

async fn new_redis_unpooled_helper(redis_dsn: &str) -> RedisPool {
    let cli = redis::cluster::ClusterClient::builder(vec![redis_dsn])
        .retries(1)
        .connection_timeout(REDIS_CONN_TIMEOUT)
        .build()
        .expect("Error initializing redis-unpooled client");
    let con = cli
        .get_async_connection()
        .await
        .expect("Failed to get redis-cluster-unpooled connection");
    RedisPool::ClusteredUnpooled(ClusteredRedisUnpooled { con })
}

pub async fn new_redis_pool_clustered(redis_dsn: &str, cfg: &Configuration) -> RedisPool {
    new_redis_pool_helper(redis_dsn, true, cfg.redis_pool_max_size).await
}

pub async fn new_redis_clustered_unpooled(redis_dsn: &str) -> RedisPool {
    new_redis_unpooled_helper(redis_dsn).await
}

pub async fn new_redis_pool(redis_dsn: &str, cfg: &Configuration) -> RedisPool {
    new_redis_pool_helper(redis_dsn, false, cfg.redis_pool_max_size).await
}

#[cfg(test)]
mod tests {
    use redis::AsyncCommands;

    use super::RedisPool;
    use crate::cfg::{CacheType, Configuration};

    async fn get_pool(redis_dsn: &str, cfg: &Configuration) -> RedisPool {
        match cfg.cache_type {
            CacheType::RedisClusterUnpooled => super::new_redis_clustered_unpooled(redis_dsn).await,
            CacheType::Redis => super::new_redis_pool(redis_dsn, cfg).await,
            _ => panic!(
                "This test should only be run when redis is configured as the cache provider"
            ),
        }
    }

    // Ensure basic set/get works -- should test sharding as well:
    #[tokio::test]
    // run with `cargo test -- --ignored redis` only when redis is up and configured
    #[ignore]
    async fn test_set_read_random_keys() {
        dotenvy::dotenv().ok();
        let cfg = crate::cfg::load().unwrap();

        let pool = get_pool(cfg.redis_dsn.as_ref().unwrap().as_str(), &cfg).await;
        let mut conn = pool.get().await.unwrap();

        for (val, key) in "abcdefghijklmnopqrstuvwxyz".chars().enumerate() {
            let key = key.to_string();
            let _: () = conn.set(key.clone(), val).await.unwrap();
            assert_eq!(conn.get::<_, usize>(&key).await.unwrap(), val);
        }
    }
}
