use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FilterOptions {
    pub file_types: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub include_hidden: bool,
    pub max_results: u32,
}