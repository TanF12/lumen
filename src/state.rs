use crate::config::Config;
use bytes::Bytes;
use lru::LruCache;
use minijinja::Environment;
use std::{
    collections::hash_map::RandomState,
    hash::BuildHasher,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, RwLock, atomic::AtomicBool},
    time::SystemTime,
};

#[derive(Clone)]
pub struct CacheEntry {
    pub raw: Bytes,
    pub br: Arc<OnceLock<Bytes>>,
    pub gz: Arc<OnceLock<Bytes>>,
    pub content_type: String,
    pub mtime: SystemTime,
}

impl CacheEntry {
    pub fn size_bytes(&self) -> usize {
        self.raw.len()
            + self.br.get().map(|v| v.len()).unwrap_or(0)
            + self.gz.get().map(|v| v.len()).unwrap_or(0)
    }
}

const SHARDS: usize = 16;

pub struct CacheShard<K, V> {
    pub cache: LruCache<K, V, RandomState>,
    pub current_bytes: usize,
    pub max_bytes: usize,
    pub max_entries: usize,
}

pub struct ShardedLruCache<K, V> {
    pub shards: Vec<Mutex<CacheShard<K, V>>>,
    pub builder: RandomState,
}

impl<K: std::hash::Hash + Eq, V: Clone> ShardedLruCache<K, V> {
    pub fn new(max_total_bytes: usize, max_total_entries: usize) -> Self {
        let shard_max_bytes = std::cmp::max(1, max_total_bytes / SHARDS);
        let shard_max_entries = std::cmp::max(1, max_total_entries / SHARDS);
        let builder = RandomState::new();

        let mut shards = Vec::with_capacity(SHARDS);
        for _ in 0..SHARDS {
            shards.push(Mutex::new(CacheShard {
                cache: LruCache::unbounded_with_hasher(builder.clone()),
                current_bytes: 0,
                max_bytes: shard_max_bytes,
                max_entries: shard_max_entries,
            }));
        }
        Self { shards, builder }
    }

    #[inline(always)]
    pub fn get_shard(&self, k: &K) -> usize {
        (self.builder.hash_one(k) as usize) % SHARDS
    }

    pub fn get(&self, k: &K) -> Option<V> {
        let shard_idx = self.get_shard(k);
        let mut shard = self.shards[shard_idx]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shard.cache.get(k).cloned()
    }

    pub fn clear(&self) {
        for shard in &self.shards {
            let mut s = shard.lock().unwrap_or_else(|e| e.into_inner());
            s.cache.clear();
            s.current_bytes = 0;
        }
    }
}

pub struct ServerState {
    pub base_dir: PathBuf,
    pub base_canon: PathBuf,
    pub page_cache: ShardedLruCache<PathBuf, CacheEntry>,
    pub dir_cache: ShardedLruCache<PathBuf, (u64, minijinja::Value)>,
    pub theme_state: RwLock<(u64, Arc<Environment<'static>>)>,
    pub config: Config,
    pub precomputed_headers: Arc<[u8]>,
    pub is_running: Arc<AtomicBool>,
}

impl ServerState {
    pub fn cache_put(&self, path: PathBuf, entry: CacheEntry) {
        let shard_idx = self.page_cache.get_shard(&path);
        let size = entry.size_bytes();

        let mut shard = self.page_cache.shards[shard_idx]
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if let Some(old_val) = shard.cache.peek(&path) {
            shard.current_bytes = shard.current_bytes.saturating_sub(old_val.size_bytes());
        }

        shard.current_bytes += size;
        shard.cache.put(path, entry);

        while (shard.current_bytes > shard.max_bytes || shard.cache.len() > shard.max_entries)
            && !shard.cache.is_empty()
        {
            if let Some((_, evicted)) = shard.cache.pop_lru() {
                shard.current_bytes = shard.current_bytes.saturating_sub(evicted.size_bytes());
            }
        }
    }

    pub fn add_cache_size(&self, path: &PathBuf, additional_bytes: usize) {
        let shard_idx = self.page_cache.get_shard(path);
        let mut shard = self.page_cache.shards[shard_idx]
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if shard.cache.peek(path).is_some() {
            shard.current_bytes += additional_bytes;

            while (shard.current_bytes > shard.max_bytes || shard.cache.len() > shard.max_entries)
                && !shard.cache.is_empty()
            {
                if let Some((_, evicted)) = shard.cache.pop_lru() {
                    shard.current_bytes = shard.current_bytes.saturating_sub(evicted.size_bytes());
                }
            }
        }
    }

    pub fn dir_cache_put(&self, path: PathBuf, hash: u64, val: minijinja::Value) {
        let shard_idx = self.dir_cache.get_shard(&path);
        let mut shard = self.dir_cache.shards[shard_idx]
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        shard.cache.put(path, (hash, val));

        while shard.cache.len() > shard.max_entries && !shard.cache.is_empty() {
            shard.cache.pop_lru();
        }
    }
}
