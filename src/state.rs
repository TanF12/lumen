use crate::config::Config;
use lru::LruCache;
use minijinja::Environment;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock, atomic::AtomicUsize},
    time::SystemTime,
};

#[derive(Clone)]
pub struct CacheEntry {
    pub html: Arc<String>,
    pub mtime: SystemTime,
}

const SHARDS: usize = 16;

pub struct ShardedLruCache<K, V> {
    shards: Vec<Mutex<LruCache<K, V>>>,
}

impl<K: Hash + Eq, V: Clone> ShardedLruCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        let shard_cap = std::cmp::max(1, capacity / SHARDS);
        let mut shards = Vec::with_capacity(SHARDS);
        for _ in 0..SHARDS {
            shards.push(Mutex::new(LruCache::new(
                NonZeroUsize::new(shard_cap).unwrap(),
            )));
        }
        Self { shards }
    }

    #[inline(always)]
    fn get_shard(&self, k: &K) -> usize {
        let mut hasher = DefaultHasher::new();
        k.hash(&mut hasher);
        (hasher.finish() as usize) % SHARDS
    }

    pub fn get(&self, k: &K) -> Option<V> {
        let shard_idx = self.get_shard(k);
        let mut shard = self.shards[shard_idx]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shard.get(k).cloned()
    }

    pub fn put(&self, k: K, v: V) {
        let shard_idx = self.get_shard(&k);
        let mut shard = self.shards[shard_idx]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shard.put(k, v);
    }

    pub fn clear(&self) {
        for shard in &self.shards {
            shard.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }
}

pub struct ServerState {
    pub base_dir: PathBuf,
    pub page_cache: ShardedLruCache<PathBuf, CacheEntry>,
    pub theme_state: RwLock<(SystemTime, Arc<Environment<'static>>)>,
    pub config: Config,
    pub precomputed_headers: String,
    pub active_connections: AtomicUsize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::SystemTime;

    #[test]
    fn test_sharded_lru_cache() {
        let cache: ShardedLruCache<PathBuf, CacheEntry> = ShardedLruCache::new(32);
        let path = PathBuf::from("test.md");

        let entry = CacheEntry {
            html: Arc::new("<h1>Cached</h1>".to_string()),
            mtime: SystemTime::now(),
        };

        cache.put(path.clone(), entry.clone());
        let retrieved = cache.get(&path).expect("Item should be in cache");
        assert_eq!(*retrieved.html, *entry.html);

        cache.clear();
        assert!(
            cache.get(&path).is_none(),
            "Cache should be empty after clear"
        );
    }
}
