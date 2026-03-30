use crate::search::SearchEngine;
use crate::state::AppState;
use crate::types::{FilterOptions, SearchResult};
use std::sync::Arc;

#[tauri::command]
pub async fn search(
    state: tauri::State<'_, Arc<AppState>>,
    query: String,
    filters: FilterOptions,
) -> Result<Vec<SearchResult>, String> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(vec![]);
    }

    let index = match state.index.load() {
        Some(idx) => idx,
        None => return Ok(vec![]),
    };

    Ok(SearchEngine::search(&index, q, &filters))
}