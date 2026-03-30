use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DriveInfo {
    pub letter: char,
    pub drive_type: String,
    pub available: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DriveProgress {
    pub letter: char,
    pub scanned: u64,
    pub finished: bool,
    pub method: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IndexState {
    pub running: bool,
    pub drives: Vec<DriveProgress>,
    pub total_scanned: u64,
    pub total_files: u64,
    pub total_dirs: u64,
    pub elapsed_ms: u64,
    pub finished: bool,
}

impl IndexState {
    pub fn new() -> Self {
        Self {
            running: false,
            drives: Vec::new(),
            total_scanned: 0,
            total_files: 0,
            total_dirs: 0,
            elapsed_ms: 0,
            finished: false,
        }
    }
}