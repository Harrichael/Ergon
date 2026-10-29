pub mod claude;
pub mod db;
pub mod derived;
pub mod export;
pub mod fmt;
pub mod gantt;
pub mod kanban;
pub mod landings;
pub mod ops;
pub mod outline;
pub mod picker;
pub mod project;
pub mod refs;
pub mod slug;
pub mod task;
pub mod view;

use rand::SeedableRng;
use rand::rngs::StdRng;

/// Current time in unix seconds, or `$ERGON_NOW` if set (for tests).
pub fn now() -> i64 {
    if let Some(t) = std::env::var("ERGON_NOW").ok().and_then(|t| t.parse().ok()) {
        return t;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64
}

/// Randomness for slugs and UUIDs. `$ERGON_SEED` makes it deterministic (for
/// tests); `salt` then keeps separate draws, like two projects' UUIDs, apart.
pub fn rng(salt: u64) -> StdRng {
    match std::env::var("ERGON_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(seed) => StdRng::seed_from_u64(seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
        None => rand::make_rng(),
    }
}

/// A random (version 4) UUID.
pub fn uuid(rng: &mut impl rand::Rng) -> String {
    let mut bytes = [0u8; 16];
    rng.fill_bytes(&mut bytes);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

/// Where `program` is found on `PATH`.
pub(crate) fn on_path(program: &std::path::Path) -> Option<std::path::PathBuf> {
    let dirs = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&dirs)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}
