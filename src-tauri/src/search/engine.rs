use super::scoring::calculate_score;
use crate::index::store::Index;
use crate::types::{FilterOptions, SearchResult};
use memchr::memmem::Finder;
use rayon::prelude::*;
use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use time::format_description::FormatItem;
use time::macros::format_description;

struct ScoredEntry {
    index: usize,
    score: f32,
}

impl PartialEq for ScoredEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Eq for ScoredEntry {}

impl PartialOrd for ScoredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other
            .score
            .partial_cmp(&self.score)
            .unwrap_or(CmpOrdering::Equal)
    }
}

pub struct SearchEngine;

impl SearchEngine {
    pub fn search(index: &Arc<Index>, query: &str, filters: &FilterOptions) -> Vec<SearchResult> {
        let q_lower = query.to_lowercase();
        let q_bytes = q_lower.as_bytes();
        let max_results = filters.max_results as usize;

        let finder = Finder::new(q_bytes);

        let type_filters: Vec<Vec<u8>> = filters
            .file_types
            .iter()
            .map(|ft| ft.trim().trim_start_matches('.').to_lowercase())
            .filter(|ft| !ft.is_empty())
            .map(|ft| ft.into_bytes())
            .collect();

        let has_type_filter = !type_filters.is_empty();

        let candidates = Self::collect_candidates(index, q_bytes, &type_filters, has_type_filter);

        let scored: Vec<(usize, f32)> = candidates
            .into_par_iter()
            .filter_map(|id| {
                let entry = &index.entries[id];

                if !filters.include_hidden && entry.is_hidden() {
                    return None;
                }

                if !entry.is_dir() {
                    if let Some(min_size) = filters.min_size {
                        if entry.size < min_size {
                            return None;
                        }
                    }
                    if let Some(max_size) = filters.max_size {
                        if entry.size > max_size {
                            return None;
                        }
                    }
                }

                let name_lower = index.entry_name_lower(entry);
                let match_pos = finder.find(name_lower)?;

                if has_type_filter && !entry.is_dir() {
                    let has_ext = name_lower
                        .iter()
                        .rposition(|&b| b == b'.')
                        .map(|dot_pos| {
                            let ext = &name_lower[dot_pos + 1..];
                            type_filters.iter().any(|tf| tf.as_slice() == ext)
                        })
                        .unwrap_or(false);

                    if !has_ext {
                        return None;
                    }
                }

                let score = calculate_score(name_lower.len(), q_bytes.len(), match_pos);
                Some((id, score))
            })
            .collect();

        let top = Self::top_k(scored, max_results);

        top.into_iter()
            .map(|(id, score)| {
                let entry = &index.entries[id];
                let name = index.entry_name(entry);
                let path = index.entry_path(entry);

                let modified_str = if entry.modified > 0 {
                    let st = UNIX_EPOCH + Duration::from_secs(entry.modified);
                    Self::format_time(st).unwrap_or_default()
                } else {
                    String::new()
                };

                SearchResult {
                    name: name.to_string(),
                    path: path.to_string(),
                    size: entry.size,
                    modified: modified_str,
                    is_dir: entry.is_dir(),
                    score: Some(score),
                }
            })
            .collect()
    }

    fn collect_candidates(
        index: &Index,
        q_bytes: &[u8],
        type_filters: &[Vec<u8>],
        has_type_filter: bool,
    ) -> Vec<usize> {
        if let Some(trigram_candidates) = index.trigram.query(q_bytes) {
            if has_type_filter {
                let mut ext_ids: Vec<usize> = Vec::new();
                for tf in type_filters {
                    if let Some(ids) = index.ext_map.get(tf.as_slice()) {
                        ext_ids.extend(ids.iter().map(|&id| id as usize));
                    }
                }
                ext_ids.sort_unstable();
                ext_ids.dedup();

                let trigram_usize: Vec<usize> =
                    trigram_candidates.iter().map(|&id| id as usize).collect();

                let mut result = intersect_sorted_usize(&trigram_usize, &ext_ids);

                for i in 0..index.entries.len() {
                    if index.entries[i].is_dir() {
                        if trigram_usize.binary_search(&i).is_ok() {
                            result.push(i);
                        }
                    }
                }
                result.sort_unstable();
                result.dedup();
                result
            } else {
                trigram_candidates.iter().map(|&id| id as usize).collect()
            }
        } else if has_type_filter {
            let mut ids: Vec<usize> = Vec::new();
            for tf in type_filters {
                if let Some(ext_ids) = index.ext_map.get(tf.as_slice()) {
                    ids.extend(ext_ids.iter().map(|&id| id as usize));
                }
            }
            for i in 0..index.entries.len() {
                if index.entries[i].is_dir() {
                    ids.push(i);
                }
            }
            ids.sort_unstable();
            ids.dedup();
            ids
        } else {
            (0..index.entries.len()).collect()
        }
    }

    fn top_k(scored: Vec<(usize, f32)>, k: usize) -> Vec<(usize, f32)> {
        if scored.len() <= k {
            let mut result = scored;
            result.sort_unstable_by(|a, b| {
                b.1.partial_cmp(&a.1).unwrap_or(CmpOrdering::Equal)
            });
            return result;
        }

        let mut heap: BinaryHeap<ScoredEntry> = BinaryHeap::with_capacity(k + 1);

        for (index, score) in scored {
            heap.push(ScoredEntry { index, score });
            if heap.len() > k {
                heap.pop();
            }
        }

        let mut result: Vec<(usize, f32)> =
            heap.into_iter().map(|se| (se.index, se.score)).collect();

        result.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(CmpOrdering::Equal));

        result
    }

    fn format_time(st: std::time::SystemTime) -> Option<String> {
        const FMT: &[FormatItem<'static>] =
            format_description!("[year]-[month]-[day] [hour]:[minute]");
        let dur = st.duration_since(UNIX_EPOCH).ok()?;
        let odt = time::OffsetDateTime::from_unix_timestamp(dur.as_secs() as i64).ok()?;
        Some(odt.format(FMT).ok()?)
    }
}

fn intersect_sorted_usize(a: &[usize], b: &[usize]) -> Vec<usize> {
    let mut result = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            CmpOrdering::Less => i += 1,
            CmpOrdering::Greater => j += 1,
            CmpOrdering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result
}