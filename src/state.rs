use crate::config::Config;
use lru::LruCache;
use minijinja::Environment;
use rustls::ServerConfig as RustlsConfig;
use std::{
    hash::{BuildHasherDefault, Hash, Hasher},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize},
    },
    time::SystemTime,
};

#[derive(Clone)]
pub struct CacheEntry {
    pub html: Arc<String>,
    pub mtime: SystemTime,
}

pub struct FxHasher(u64);
impl Default for FxHasher {
    #[inline(always)]
    fn default() -> Self {
        Self(0x517cc1b727220a95)
    }
}
impl Hasher for FxHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ (b as u64)).wrapping_mul(0x517cc1b727220a95);
        }
    }
}

const SHARDS: usize = 16;

pub struct ShardedLruCache<K, V> {
    shards: Vec<Mutex<LruCache<K, V, BuildHasherDefault<FxHasher>>>>,
}

impl<K: Hash + Eq, V: Clone> ShardedLruCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        let shard_cap = std::cmp::max(1, capacity / SHARDS);
        let mut shards = Vec::with_capacity(SHARDS);
        for _ in 0..SHARDS {
            shards.push(Mutex::new(LruCache::with_hasher(
                NonZeroUsize::new(shard_cap).unwrap(),
                BuildHasherDefault::<FxHasher>::default(),
            )));
        }
        Self { shards }
    }

    #[inline(always)]
    fn get_shard(&self, k: &K) -> usize {
        let mut hasher = FxHasher::default();
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
    pub dir_cache: ShardedLruCache<PathBuf, (u64, minijinja::Value)>,
    pub theme_state: RwLock<(u64, Arc<Environment<'static>>)>,
    pub config: Config,
    pub precomputed_headers: Arc<[u8]>,
    pub active_connections: AtomicUsize,
    pub tls_config: Option<Arc<RustlsConfig>>,
    pub is_running: Arc<AtomicBool>,
}
