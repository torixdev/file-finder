use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub modified: String,
    pub is_dir: bool,
    pub score: Option<f32>,
}