//! Shared fixtures for the core integration tests: an isolated store per test,
//! never the owner's database (AGENTS.md rule 1).
#![allow(dead_code)] // Each test binary uses a different subset.
use rusqlite::{params, Connection};
use rustmedia_core::Core;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-data")
}

/// A fresh data directory under `target/test-data`, removed on drop.
pub struct Data(pub PathBuf);
impl Data {
    pub fn new() -> Self {
        let path = root().join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    pub fn core(&self) -> Core {
        Core::open(&self.0).unwrap()
    }
    pub fn db(&self) -> Connection {
        Connection::open(self.0.join("core.sqlite3")).unwrap()
    }
    /// Registers an addon directly, without fetching its manifest.
    pub fn install(&self, url: &str, manifest: &Value) {
        self.db()
            .execute(
                "INSERT INTO addons(url,manifest,generation) VALUES(?1,?2,'test')",
                params![url, manifest.to_string()],
            )
            .unwrap();
    }
}
impl Drop for Data {
    fn drop(&mut self) {
        assert!(self.0.is_absolute() && self.0.starts_with(root()));
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
