pub mod db;
pub mod derived;
pub mod fmt;
pub mod ops;
pub mod project;
pub mod refs;
pub mod slug;
pub mod task;
pub mod view;

/// Current time in unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64
}
