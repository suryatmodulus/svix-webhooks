// SPDX-FileCopyrightText: © 2022 Svix Authors
// SPDX-License-Identifier: MIT

use std::time::Duration;

use enum_dispatch::enum_dispatch;

use super::kv_backend::{
    memory::MemoryKv, none::NoKvBackend, redis::RedisKv, Key, KvBackend, StringValue, Value,
};

#[derive(Clone)]
#[enum_dispatch(KvBackend)]
pub enum SharedStore {
    MemorySharedStore(MemoryKv),
    RedisSharedStore(RedisKv),
    None(NoKvBackend),
}

impl SharedStore {
    pub fn is_none(&self) -> bool {
        matches!(*self, SharedStore::None(_))
    }
}

impl SharedStoreBehavior for SharedStore {}

pub trait SharedStoreBehavior: KvBackend {}
