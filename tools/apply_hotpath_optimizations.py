from pathlib import Path


def replace_exact(path, old, new, expected=1):
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != expected:
        raise SystemExit(f'{path}: expected {expected} occurrence(s), found {count}: {old!r}')
    file.write_text(text.replace(old, new))


replace_exact('src/limits.rs', 'use dashmap::DashMap;\n', 'use ahash::RandomState;\nuse dashmap::DashMap;\n')
replace_exact('src/limits.rs', '#[derive(Clone, Eq)]\nstruct ClientKey', '#[derive(Clone, Copy, Eq)]\nstruct ClientKey')
replace_exact('src/limits.rs', 'buckets: DashMap<ClientKey, Bucket>,', 'buckets: DashMap<ClientKey, Bucket, RandomState>,')
replace_exact('src/limits.rs', 'counters: DashMap<ClientKey, Arc<AtomicUsize>>,', 'counters: DashMap<ClientKey, Arc<AtomicUsize>, RandomState>,')
replace_exact('src/limits.rs', 'buckets: DashMap::new(),', 'buckets: DashMap::with_hasher(RandomState::new()),')
replace_exact('src/limits.rs', 'counters: DashMap::new(),', 'counters: DashMap::with_hasher(RandomState::new()),')
replace_exact('src/limits.rs', 'let entry = self.counters.entry(key.clone());', 'let entry = self.counters.entry(key);')

replace_exact('src/h3_wire.rs', 'use std::collections::HashMap;\n\nuse bytes::Bytes;', 'use ahash::AHashMap;\nuse bytes::Bytes;')
replace_exact('src/h3_wire.rs', 'HashMap::with_capacity', 'AHashMap::with_capacity', expected=2)

replace_exact('src/static_files.rs', 'use ahash::AHasher;', 'use ahash::{AHashMap, AHasher};')
replace_exact('src/static_files.rs', 'entries: HashMap<String, Mutex<LruCache<String, CachedPath>>>,', 'entries: AHashMap<String, Mutex<LruCache<String, CachedPath>>>,')
replace_exact('src/static_files.rs', 'pub struct StaticFiles {\n    roots: HashMap<String, PathBuf>,', 'pub struct StaticFiles {\n    roots: AHashMap<String, PathBuf>,')
replace_exact('src/static_files.rs', 'let mut canonical_roots = HashMap::with_capacity(roots.len());', 'let mut canonical_roots = AHashMap::with_capacity(roots.len());')

replace_exact('src/upstream_h3.rs', 'use std::collections::{HashMap, VecDeque};', 'use std::collections::VecDeque;')
replace_exact('src/upstream_h3.rs', 'routes: HashMap<String, H3Route>,\n    pools: HashMap<String, Arc<H3Pool>>,', 'routes: AHashMap<String, H3Route>,\n    pools: AHashMap<String, Arc<H3Pool>>,')
replace_exact('src/upstream_h3.rs', 'let mut routes = HashMap::new();', 'let mut routes = AHashMap::new();')
replace_exact('src/upstream_h3.rs', 'let mut pools = HashMap::new();', 'let mut pools = AHashMap::new();')
replace_exact(
    'src/upstream_h3.rs',
    '    fn select_shard(&self) -> &PoolShard {\n        let index = self.round_robin.fetch_add(1, Ordering::Relaxed);\n        &self.shards[index % self.shards.len()]\n    }',
    '    fn select_shard(&self) -> &PoolShard {\n        // One connection is the default and common deployment. Avoid both the\n        // shared atomic RMW and integer modulo when round-robin cannot change\n        // the selected shard.\n        if self.shards.len() == 1 {\n            return &self.shards[0];\n        }\n        let index = self.round_robin.fetch_add(1, Ordering::Relaxed);\n        &self.shards[index % self.shards.len()]\n    }',
)
