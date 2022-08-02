// SPDX-FileCopyrightText: © 2022 Svix Authors
// SPDX-License-Identifier: MIT

use enum_dispatch::enum_dispatch;

use super::kv_backend::{memory::MemoryKv, none::NoKvBackend, redis::RedisKv, KvBackend};

#[derive(Clone)]
#[enum_dispatch(KvBackend)]
pub enum Cache {
    MemoryCache(MemoryKv),
    RedisCache(RedisKv),
    None(NoKvBackend),
}

impl Cache {
    pub fn is_none(&self) -> bool {
        matches!(*self, Cache::None(_))
    }
}

impl CacheBehavior for Cache {}

pub trait CacheBehavior: KvBackend {}
