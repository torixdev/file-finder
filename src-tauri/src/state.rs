use crate::index::store::IndexStore;
use crate::types::IndexState;
use parking_lot::RwLock;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tauri::Emitter;

pub struct AppState {
    pub index: IndexStore,
    pub cancel_flag: AtomicBool,
    pub stats_files: AtomicU64,
    pub stats_dirs: AtomicU64,
    pub stats_scanned: AtomicU64,
    pub index_state: RwLock<IndexState>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            index: IndexStore::new(),
            cancel_flag: AtomicBool::new(false),
            stats_files: AtomicU64::new(0),
            stats_dirs: AtomicU64::new(0),
            stats_scanned: AtomicU64::new(0),
            index_state: RwLock::new(IndexState::new()),
        }
    }

    pub fn reset_stats(&self) {
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.stats_files.store(0, Ordering::Relaxed);
        self.stats_dirs.store(0, Ordering::Relaxed);
        self.stats_scanned.store(0, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::Relaxed)
    }

    pub fn emit_state(&self, app: &tauri::AppHandle) {
        let mut state = self.index_state.write();
        state.total_scanned = self.stats_scanned.load(Ordering::Relaxed);
        state.total_files = self.stats_files.load(Ordering::Relaxed);
        state.total_dirs = self.stats_dirs.load(Ordering::Relaxed);
        let state_clone = state.clone();
        drop(state);
        let _ = app.emit("index:state", state_clone);
    }
}