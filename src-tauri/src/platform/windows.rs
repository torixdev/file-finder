use crate::index::builder::IndexBuilder;
use crate::mft::scanner::MftScanResult;
use crate::mft::types::{FILE_REF_MASK, ROOT_FILE_REF};
use crate::types::DriveInfo;
use parking_lot::Mutex;
use rayon::prelude::*;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::UNIX_EPOCH;
use winapi::um::fileapi::GetDriveTypeW;
use winapi::um::winbase::{DRIVE_FIXED, DRIVE_REMOVABLE};

pub fn get_drive_type_string(drive_letter: char) -> String {
    unsafe {
        let path = format!("{}:\\", drive_letter);
        let wide: Vec<u16> = OsStr::new(&path).encode_wide().chain(Some(0)).collect();
        match GetDriveTypeW(wide.as_ptr()) {
            DRIVE_FIXED => "Локальный диск".to_string(),
            DRIVE_REMOVABLE => "Съёмный диск".to_string(),
            _ => "Другой".to_string(),
        }
    }
}

pub fn list_available_drives() -> Vec<DriveInfo> {
    (b'C'..=b'Z')
        .filter_map(|letter| {
            let ch = letter as char;
            let drive = format!("{}:\\", ch);
            if Path::new(&drive).exists() {
                Some(DriveInfo {
                    letter: ch,
                    drive_type: get_drive_type_string(ch),
                    available: true,
                })
            } else {
                None
            }
        })
        .collect()
}

pub fn build_paths_into_builder(
    scan_result: MftScanResult,
    drive_letter: char,
    files_count: &AtomicU64,
    dirs_count: &AtomicU64,
) -> IndexBuilder {
    let raw_entries = scan_result.entries;
    let arena = scan_result.arena;

    if raw_entries.is_empty() {
        return IndexBuilder::with_capacity(0);
    }

    let root_ref_masked = ROOT_FILE_REF & FILE_REF_MASK;

    let mut sorted_refs: Vec<(u64, u32)> = raw_entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.file_ref, i as u32))
        .collect();
    sorted_refs.sort_unstable_by_key(|&(r, _)| r);

    let child_count = {
        let mut counts = vec![0u32; raw_entries.len()];
        for entry in &raw_entries {
            if entry.parent_ref != root_ref_masked && entry.parent_ref != 0 {
                if let Ok(pos) = sorted_refs.binary_search_by_key(&entry.parent_ref, |&(r, _)| r) {
                    let parent_idx = sorted_refs[pos].1 as usize;
                    counts[parent_idx] += 1;
                }
            }
        }
        counts
    };

    let mut offsets = vec![0u32; raw_entries.len() + 1];
    for i in 0..raw_entries.len() {
        offsets[i + 1] = offsets[i] + child_count[i];
    }

    let total_children = offsets[raw_entries.len()] as usize;
    let mut children_flat = vec![0u32; total_children];
    let mut fill = offsets.clone();

    let mut roots: Vec<u32> = Vec::new();

    for (idx, entry) in raw_entries.iter().enumerate() {
        if entry.parent_ref == root_ref_masked || entry.parent_ref == 0 {
            roots.push(idx as u32);
        } else if let Ok(pos) =
            sorted_refs.binary_search_by_key(&entry.parent_ref, |&(r, _)| r)
        {
            let parent_idx = sorted_refs[pos].1 as usize;
            let slot = fill[parent_idx] as usize;
            children_flat[slot] = idx as u32;
            fill[parent_idx] += 1;
        } else {
            roots.push(idx as u32);
        }
    }

    drop(sorted_refs);
    drop(fill);
    drop(child_count);

    let root_prefix = format!("{}:\\", drive_letter);
    let root_prefix_bytes = root_prefix.as_bytes();

    if roots.len() >= 16 {
        let chunk_size = (roots.len() + 7) / 8;
        let chunks: Vec<&[u32]> = roots.chunks(chunk_size).collect();

        let sub_builders: Vec<IndexBuilder> = chunks
            .into_par_iter()
            .map(|root_chunk| {
                let estimate = raw_entries.len() / 8;
                let mut sub = IndexBuilder::with_capacity(estimate);
                let mut path_buf: Vec<u8> = Vec::with_capacity(512);
                let mut stack: Vec<(u32, usize)> = Vec::with_capacity(4096);

                for &root_idx in root_chunk.iter().rev() {
                    stack.push((root_idx, root_prefix_bytes.len()));
                }

                path_buf.extend_from_slice(root_prefix_bytes);

                while let Some((idx, parent_path_len)) = stack.pop() {
                    let idx_usize = idx as usize;
                    let entry = &raw_entries[idx_usize];
                    let name = arena.get(entry.name_off, entry.name_len);

                    path_buf.truncate(parent_path_len);
                    path_buf.extend_from_slice(name.as_bytes());

                    let full_path_len = path_buf.len();

                    if entry.is_dir {
                        dirs_count.fetch_add(1, Ordering::Relaxed);
                    } else {
                        files_count.fetch_add(1, Ordering::Relaxed);
                    }

                    sub.add_from_bytes(
                        name,
                        &path_buf[..full_path_len],
                        entry.is_dir,
                        entry.is_hidden,
                        0,
                        entry.modified,
                    );

                    let children_start = offsets[idx_usize] as usize;
                    let children_end = offsets[idx_usize + 1] as usize;

                    if entry.is_dir && children_start < children_end {
                        path_buf.push(b'\\');
                        let child_prefix_len = path_buf.len();

                        for ci in (children_start..children_end).rev() {
                            stack.push((children_flat[ci], child_prefix_len));
                        }
                    }
                }

                sub
            })
            .collect();

        let mut merged = IndexBuilder::with_capacity(raw_entries.len());
        for sub in sub_builders {
            merged.merge(sub);
        }
        merged
    } else {
        let mut builder = IndexBuilder::with_capacity(raw_entries.len());
        let mut path_buf: Vec<u8> = Vec::with_capacity(512);
        let mut stack: Vec<(u32, usize)> = Vec::with_capacity(4096);

        for &root_idx in roots.iter().rev() {
            stack.push((root_idx, root_prefix_bytes.len()));
        }

        path_buf.extend_from_slice(root_prefix_bytes);

        while let Some((idx, parent_path_len)) = stack.pop() {
            let idx_usize = idx as usize;
            let entry = &raw_entries[idx_usize];
            let name = arena.get(entry.name_off, entry.name_len);

            path_buf.truncate(parent_path_len);
            path_buf.extend_from_slice(name.as_bytes());

            let full_path_len = path_buf.len();

            if entry.is_dir {
                dirs_count.fetch_add(1, Ordering::Relaxed);
            } else {
                files_count.fetch_add(1, Ordering::Relaxed);
            }

            builder.add_from_bytes(
                name,
                &path_buf[..full_path_len],
                entry.is_dir,
                entry.is_hidden,
                0,
                entry.modified,
            );

            let children_start = offsets[idx_usize] as usize;
            let children_end = offsets[idx_usize + 1] as usize;

            if entry.is_dir && children_start < children_end {
                path_buf.push(b'\\');
                let child_prefix_len = path_buf.len();

                for ci in (children_start..children_end).rev() {
                    stack.push((children_flat[ci], child_prefix_len));
                }
            }
        }

        builder
    }
}

pub fn scan_drive_fallback(
    drive: char,
    cancel_flag: &AtomicBool,
) -> Result<Vec<(String, String, bool, bool, u64, u64)>, String> {
    use ignore::WalkBuilder;

    let root = format!("{}:\\", drive);
    let results: Mutex<Vec<(String, String, bool, bool, u64, u64)>> =
        Mutex::new(Vec::with_capacity(100_000));

    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .follow_links(false)
        .threads(num_cpus::get())
        .build_parallel();

    walker.run(|| {
        let results = &results;
        let cancel = cancel_flag;
        Box::new(move |entry| {
            if cancel.load(Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }

            if let Ok(entry) = entry {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let full_path = path.to_string_lossy().to_string();
                let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                let is_hidden = name.starts_with('.');

                let (size, modified) = entry
                    .metadata()
                    .map(|meta| {
                        let sz = if is_dir { 0 } else { meta.len() };
                        let mt = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        (sz, mt)
                    })
                    .unwrap_or((0, 0));

                results
                    .lock()
                    .push((name, full_path, is_dir, is_hidden, size, modified));
            }

            ignore::WalkState::Continue
        })
    });

    Ok(results.into_inner())
}