//! Core of gdsync: auth, Drive API client, local state (SQLite), content cache and sync.

pub mod auth;
pub mod cache;
pub mod config;
pub mod db;
pub mod drive;
pub mod names;
pub mod sync;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
