use crate::index::IndexBuilder;
use crate::mft::MftScanner;
use crate::platform::{build_paths_into_builder, scan_drive_fallback};
use crate::state::AppState;
use crate::types::{DriveProgress, IndexState};
use rayon::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

#[tauri::command]
pub fn get_index_status(state: tauri::State<'_, Arc<AppState>>) -> Result<IndexState, String> {
    Ok(state.index_state.read().clone())
}

#[tauri::command]
pub async fn start_indexing(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    selected_drives: Vec<char>,
) -> Result<(), String> {
    if selected_drives.is_empty() {
        return Err("Выберите хотя бы один диск".to_string());
    }

    let state = state.inner().clone();
    state.reset_stats();

    {
        let mut idx_state = state.index_state.write();
        *idx_state = IndexState {
            running: true,
            drives: selected_drives
                .iter()
                .map(|&letter| DriveProgress {
                    letter,
                    scanned: 0,
                    finished: false,
                    method: String::new(),
                })
                .collect(),
            total_scanned: 0,
            total_files: 0,
            total_dirs: 0,
            elapsed_ms: 0,
            finished: false,
        };
    }
    state.emit_state(&app);

    tauri::async_runtime::spawn_blocking(move || {
        let start_time = Instant::now();

        let per_drive: Vec<_> = selected_drives
            .par_iter()
            .map(|&drive| {
                let app_clone = app.clone();
                let state_ref = &state;

                let mft_result = MftScanner::open(drive);

                let result: Result<DriveData, String> = match mft_result {
                    Ok(scanner) => {
                        {
                            let mut s = state_ref.index_state.write();
                            if let Some(d) = s.drives.iter_mut().find(|d| d.letter == drive) {
                                d.method = "MFT".to_string();
                            }
                        }
                        state_ref.emit_state(&app_clone);

                        scanner
                            .enumerate(&state_ref.cancel_flag)
                            .map(|scan_result| {
                                let count = scan_result.entries.len() as u64;
                                state_ref.stats_scanned.fetch_add(count, Ordering::Relaxed);

                                let builder = build_paths_into_builder(
                                    scan_result,
                                    drive,
                                    &state_ref.stats_files,
                                    &state_ref.stats_dirs,
                                );

                                {
                                    let mut s = state_ref.index_state.write();
                                    if let Some(d) =
                                        s.drives.iter_mut().find(|d| d.letter == drive)
                                    {
                                        d.finished = true;
                                        d.scanned = count;
                                    }
                                }
                                state_ref.emit_state(&app_clone);

                                DriveData::Builder(builder)
                            })
                    }
                    Err(_) => {
                        {
                            let mut s = state_ref.index_state.write();
                            if let Some(d) = s.drives.iter_mut().find(|d| d.letter == drive) {
                                d.method = "Walker".to_string();
                            }
                        }
                        state_ref.emit_state(&app_clone);

                        scan_drive_fallback(drive, &state_ref.cancel_flag).map(|entries| {
                            let count = entries.len() as u64;
                            state_ref.stats_scanned.fetch_add(count, Ordering::Relaxed);

                            {
                                let mut s = state_ref.index_state.write();
                                if let Some(d) =
                                    s.drives.iter_mut().find(|d| d.letter == drive)
                                {
                                    d.finished = true;
                                    d.scanned = count;
                                }
                            }
                            state_ref.emit_state(&app_clone);

                            DriveData::Walker(entries)
                        })
                    }
                };

                (drive, result)
            })
            .collect();

        if state.is_cancelled() {
            let mut s = state.index_state.write();
            s.running = false;
            s.finished = true;
            s.elapsed_ms = start_time.elapsed().as_millis() as u64;
            drop(s);
            state.emit_state(&app);
            return;
        }

        let total_estimate: usize = per_drive
            .iter()
            .map(|(_, r)| match r {
                Ok(DriveData::Builder(b)) => b.entry_count(),
                Ok(DriveData::Walker(entries)) => entries.len(),
                Err(_) => 0,
            })
            .sum();

        let mut final_builder = IndexBuilder::with_capacity(total_estimate);

        for (_drive, result) in per_drive {
            match result {
                Ok(DriveData::Builder(builder)) => {
                    final_builder.merge(builder);
                }
                Ok(DriveData::Walker(entries)) => {
                    for (name, path, is_dir, is_hidden, size, modified) in entries {
                        if is_dir {
                            state.stats_dirs.fetch_add(1, Ordering::Relaxed);
                        } else {
                            state.stats_files.fetch_add(1, Ordering::Relaxed);
                        }
                        final_builder.add(&name, &path, is_dir, is_hidden, size, modified);
                    }
                }
                Err(_) => {}
            }
        }

        state.emit_state(&app);

        let index = final_builder.finalize();
        state.index.store(index);

        {
            let mut s = state.index_state.write();
            s.running = false;
            s.finished = true;
            s.elapsed_ms = start_time.elapsed().as_millis() as u64;
            s.total_files = state.stats_files.load(Ordering::Relaxed);
            s.total_dirs = state.stats_dirs.load(Ordering::Relaxed);
            s.total_scanned = state.stats_scanned.load(Ordering::Relaxed);
        }
        state.emit_state(&app);
    });

    Ok(())
}

enum DriveData {
    Builder(IndexBuilder),
    Walker(Vec<(String, String, bool, bool, u64, u64)>),
}