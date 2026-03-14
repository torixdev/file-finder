#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![cfg(target_os = "windows")]

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap as StdHashMap,
    ffi::OsStr,
    fs,
    mem,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr,
    slice,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ahash::AHashMap as HashMap;
use arc_swap::ArcSwap;
use memchr::memmem::Finder;
use once_cell::sync::Lazy;
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use tauri::Emitter;
use time::{format_description::FormatItem, macros::format_description};
use winapi::{
    shared::{
        minwindef::{DWORD, LPVOID},
        ntdef::{HANDLE, LARGE_INTEGER, LONGLONG, ULONGLONG},
        winerror::ERROR_HANDLE_EOF,
    },
    um::{
        errhandlingapi::GetLastError,
        fileapi::{CreateFileW, GetDriveTypeW, OPEN_EXISTING},
        handleapi::{CloseHandle, INVALID_HANDLE_VALUE},
        ioapiset::DeviceIoControl,
        winbase::{DRIVE_FIXED, DRIVE_REMOVABLE, FILE_FLAG_BACKUP_SEMANTICS},
        winnt::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GENERIC_READ,
        },
    },
};

const FSCTL_ENUM_USN_DATA: DWORD = 0x000900b3;
const FSCTL_QUERY_USN_JOURNAL: DWORD = 0x000900f4;
const MFT_BUFFER_SIZE: usize = 16 * 1024 * 1024;
const ROOT_FILE_REF: u64 = 0x0005_0000_0000_0005;
const FILE_REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[repr(C)]
#[derive(Copy, Clone)]
struct MftEnumDataV0 {
    start_file_reference_number: ULONGLONG,
    low_usn: LONGLONG,
    high_usn: LONGLONG,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UsnJournalDataV0 {
    usn_journal_id: ULONGLONG,
    first_usn: LONGLONG,
    next_usn: LONGLONG,
    lowest_valid_usn: LONGLONG,
    max_usn: LONGLONG,
    maximum_size: ULONGLONG,
    allocation_delta: ULONGLONG,
}

#[repr(C)]
struct UsnRecordV2 {
    record_length: DWORD,
    major_version: u16,
    minor_version: u16,
    file_reference_number: ULONGLONG,
    parent_file_reference_number: ULONGLONG,
    usn: LONGLONG,
    time_stamp: LARGE_INTEGER,
    reason: DWORD,
    source_info: DWORD,
    security_id: DWORD,
    file_attributes: DWORD,
    file_name_length: u16,
    file_name_offset: u16,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FilterOptions {
    file_types: Vec<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    include_hidden: bool,
    max_results: u32,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    name: String,
    path: String,
    size: u64,
    modified: String,
    is_dir: bool,
    score: Option<f32>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DriveInfo {
    letter: char,
    drive_type: String,
    available: bool,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DriveProgress {
    letter: char,
    scanned: u64,
    finished: bool,
    method: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct IndexState {
    running: bool,
    drives: Vec<DriveProgress>,
    total_scanned: u64,
    total_files: u64,
    total_dirs: u64,
    elapsed_ms: u64,
    finished: bool,
}

#[derive(Clone)]
struct MftRawEntry {
    file_ref: u64,
    parent_ref: u64,
    name: Box<str>,
    is_dir: bool,
    is_hidden: bool,
    modified: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    name_off: u32,
    name_len: u16,
    name_lower_off: u32,
    name_lower_len: u16,
    path_off: u32,
    path_len: u32,
    size: u64,
    modified: u64,
    flags: u16,
}

impl Entry {
    #[inline(always)]
    fn is_dir(&self) -> bool {
        (self.flags & 0x01) != 0
    }

    #[inline(always)]
    fn is_hidden(&self) -> bool {
        (self.flags & 0x02) != 0
    }
}

struct Index {
    entries: Vec<Entry>,
    arena: Vec<u8>,
    ext_map: HashMap<Box<[u8]>, Vec<u32>>,
}

impl Index {
    #[inline(always)]
    fn get_bytes(&self, off: u32, len: usize) -> &[u8] {
        &self.arena[off as usize..off as usize + len]
    }

    #[inline(always)]
    fn get_str(&self, off: u32, len: usize) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.get_bytes(off, len)) }
    }

    #[inline(always)]
    fn entry_name(&self, e: &Entry) -> &str {
        self.get_str(e.name_off, e.name_len as usize)
    }

    #[inline(always)]
    fn entry_name_lower(&self, e: &Entry) -> &[u8] {
        self.get_bytes(e.name_lower_off, e.name_lower_len as usize)
    }

    #[inline(always)]
    fn entry_path(&self, e: &Entry) -> &str {
        self.get_str(e.path_off, e.path_len as usize)
    }
}

struct IndexBuilder {
    entries: Vec<Entry>,
    arena: Vec<u8>,
    extensions: Vec<(Box<[u8]>, u32)>,
}

impl IndexBuilder {
    fn with_capacity(cap: usize) -> Self {
        Self {
            entries: Vec::with_capacity(cap),
            arena: Vec::with_capacity(cap * 80),
            extensions: Vec::with_capacity(cap / 2),
        }
    }

    #[inline]
    fn intern(&mut self, s: &str) -> u32 {
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(s.as_bytes());
        off
    }

    #[inline]
    fn intern_bytes(&mut self, b: &[u8]) -> u32 {
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(b);
        off
    }

    fn add(
        &mut self,
        name: &str,
        path: &str,
        is_dir: bool,
        is_hidden: bool,
        size: u64,
        modified: u64,
    ) {
        let id = self.entries.len() as u32;

        let name_off = self.intern(name);
        let name_len = name.len().min(u16::MAX as usize) as u16;

        let name_lower = name.to_lowercase();
        let name_lower_bytes = name_lower.as_bytes();
        let name_lower_off = self.intern_bytes(name_lower_bytes);
        let name_lower_len = name_lower_bytes.len().min(u16::MAX as usize) as u16;

        let path_off = self.intern(path);
        let path_len = path.len() as u32;

        let mut flags = 0u16;
        if is_dir {
            flags |= 0x01;
        }
        if is_hidden {
            flags |= 0x02;
        }

        self.entries.push(Entry {
            name_off,
            name_len,
            name_lower_off,
            name_lower_len,
            path_off,
            path_len,
            size,
            modified,
            flags,
        });

        if !is_dir {
            if let Some(dot_pos) = name_lower.rfind('.') {
                let ext = &name_lower[dot_pos + 1..];
                if !ext.is_empty() && ext.len() <= 12 {
                    self.extensions.push((ext.as_bytes().into(), id));
                }
            }
        }
    }

    fn finalize(self) -> Index {
        let mut ext_map: HashMap<Box<[u8]>, Vec<u32>> = HashMap::with_capacity(256);
        for (ext, id) in self.extensions {
            ext_map.entry(ext).or_default().push(id);
        }

        for ids in ext_map.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }

        Index {
            entries: self.entries,
            arena: self.arena,
            ext_map,
        }
    }
}

struct MftScanner {
    drive_letter: char,
    volume_handle: HANDLE,
}

impl MftScanner {
    unsafe fn open(drive_letter: char) -> Result<Self, String> {
        let volume_path = format!("\\\\.\\{}:", drive_letter);
        let wide: Vec<u16> = OsStr::new(&volume_path)
            .encode_wide()
            .chain(Some(0))
            .collect();

        let handle = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        );

        if handle == INVALID_HANDLE_VALUE {
            return Err(format!("Cannot open volume {}", drive_letter));
        }

        Ok(Self {
            drive_letter,
            volume_handle: handle,
        })
    }

    unsafe fn query_usn_journal(&self) -> Result<UsnJournalDataV0, String> {
        let mut journal_data: UsnJournalDataV0 = mem::zeroed();
        let mut bytes_returned: DWORD = 0;

        let result = DeviceIoControl(
            self.volume_handle,
            FSCTL_QUERY_USN_JOURNAL,
            ptr::null_mut(),
            0,
            &mut journal_data as *mut _ as LPVOID,
            mem::size_of::<UsnJournalDataV0>() as DWORD,
            &mut bytes_returned,
            ptr::null_mut(),
        );

        if result == 0 {
            return Err(format!("FSCTL_QUERY_USN_JOURNAL failed: {}", GetLastError()));
        }

        Ok(journal_data)
    }

    unsafe fn enumerate_mft(&self) -> Result<Vec<MftRawEntry>, String> {
        let journal_data = self.query_usn_journal()?;

        let mut enum_data = MftEnumDataV0 {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: journal_data.next_usn,
        };

        let mut buffer = vec![0u8; MFT_BUFFER_SIZE];
        let mut raw_entries: Vec<MftRawEntry> = Vec::with_capacity(500_000);

        loop {
            if CANCEL_FLAG.load(Ordering::Relaxed) {
                return Ok(raw_entries);
            }

            let mut bytes_returned: DWORD = 0;
            let result = DeviceIoControl(
                self.volume_handle,
                FSCTL_ENUM_USN_DATA,
                &mut enum_data as *mut _ as LPVOID,
                mem::size_of::<MftEnumDataV0>() as DWORD,
                buffer.as_mut_ptr() as LPVOID,
                MFT_BUFFER_SIZE as DWORD,
                &mut bytes_returned,
                ptr::null_mut(),
            );

            if result == 0 {
                let err = GetLastError();
                if err == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(format!("MFT read error: {}", err));
            }

            if bytes_returned < 8 {
                break;
            }

            let mut offset = 8usize;
            let bytes_ret = bytes_returned as usize;

            while offset + mem::size_of::<UsnRecordV2>() <= bytes_ret {
                let record_ptr = buffer.as_ptr().add(offset) as *const UsnRecordV2;
                let record = &*record_ptr;

                if record.record_length == 0 || record.record_length > 0x10000 {
                    break;
                }

                let filename_offset = offset + record.file_name_offset as usize;
                let filename_len = (record.file_name_length / 2) as usize;

                if filename_offset + filename_len * 2 <= bytes_ret {
                    let filename_ptr = buffer.as_ptr().add(filename_offset) as *const u16;
                    let filename_slice = slice::from_raw_parts(filename_ptr, filename_len);
                    let filename = String::from_utf16_lossy(filename_slice);

                    if !filename.is_empty()
                        && filename != "."
                        && filename != ".."
                        && !filename.starts_with('$')
                    {
                        let is_dir = (record.file_attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                        let is_hidden = (record.file_attributes & FILE_ATTRIBUTE_HIDDEN) != 0;

                        let modified = {
                            let ft = *record.time_stamp.QuadPart() as u64;
                            if ft > 116444736000000000 {
                                (ft - 116444736000000000) / 10000000
                            } else {
                                0
                            }
                        };

                        raw_entries.push(MftRawEntry {
                            file_ref: record.file_reference_number & FILE_REF_MASK,
                            parent_ref: record.parent_file_reference_number & FILE_REF_MASK,
                            name: filename.into_boxed_str(),
                            is_dir,
                            is_hidden,
                            modified,
                        });
                    }
                }

                enum_data.start_file_reference_number = record.file_reference_number;
                offset += record.record_length as usize;
            }
        }

        Ok(raw_entries)
    }
}

impl Drop for MftScanner {
    fn drop(&mut self) {
        unsafe {
            if self.volume_handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.volume_handle);
            }
        }
    }
}

fn build_paths_topological(
    raw_entries: Vec<MftRawEntry>,
    drive_letter: char,
) -> Vec<(String, String, bool, bool, u64)> {
    if raw_entries.is_empty() {
        return Vec::new();
    }

    let root_ref_masked = ROOT_FILE_REF & FILE_REF_MASK;

    let ref_to_idx: HashMap<u64, usize> = raw_entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.file_ref, i))
        .collect();

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); raw_entries.len()];
    let mut roots: Vec<usize> = Vec::new();

    for (idx, entry) in raw_entries.iter().enumerate() {
        if entry.parent_ref == root_ref_masked || entry.parent_ref == 0 {
            roots.push(idx);
        } else if let Some(&parent_idx) = ref_to_idx.get(&entry.parent_ref) {
            children[parent_idx].push(idx);
        } else {
            roots.push(idx);
        }
    }

    let mut paths: Vec<String> = vec![String::new(); raw_entries.len()];
    let root_prefix = format!("{}:\\", drive_letter);

    let mut stack: Vec<(usize, String)> = roots
        .into_iter()
        .map(|idx| (idx, root_prefix.clone()))
        .collect();

    while let Some((idx, parent_path)) = stack.pop() {
        let entry = &raw_entries[idx];
        let mut path = String::with_capacity(parent_path.len() + entry.name.len() + 1);
        path.push_str(&parent_path);
        path.push_str(&entry.name);

        if entry.is_dir && !children[idx].is_empty() {
            let path_with_sep = format!("{}\\", path);
            for &child_idx in &children[idx] {
                stack.push((child_idx, path_with_sep.clone()));
            }
        }

        paths[idx] = path;
    }

    raw_entries
        .into_iter()
        .zip(paths.into_iter())
        .map(|(e, path)| (e.name.to_string(), path, e.is_dir, e.is_hidden, e.modified))
        .collect()
}

fn scan_drive_fallback(drive: char) -> Result<Vec<(String, String, bool, bool, u64, u64)>, String> {
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
        Box::new(move |entry| {
            if CANCEL_FLAG.load(Ordering::Relaxed) {
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

static INDEX: Lazy<ArcSwap<Option<Arc<Index>>>> = Lazy::new(|| ArcSwap::new(Arc::new(None)));

static STATS_FILES: AtomicU64 = AtomicU64::new(0);
static STATS_DIRS: AtomicU64 = AtomicU64::new(0);
static STATS_SCANNED: AtomicU64 = AtomicU64::new(0);

static INDEX_STATE: Lazy<RwLock<IndexState>> = Lazy::new(|| {
    RwLock::new(IndexState {
        running: false,
        drives: Vec::new(),
        total_scanned: 0,
        total_files: 0,
        total_dirs: 0,
        elapsed_ms: 0,
        finished: false,
    })
});

static CANCEL_FLAG: AtomicBool = AtomicBool::new(false);

fn get_drive_type_string(drive_letter: char) -> String {
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

fn list_available_drives() -> Vec<DriveInfo> {
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

fn emit_state(app: &tauri::AppHandle) {
    let mut state = INDEX_STATE.write();
    state.total_scanned = STATS_SCANNED.load(Ordering::Relaxed);
    state.total_files = STATS_FILES.load(Ordering::Relaxed);
    state.total_dirs = STATS_DIRS.load(Ordering::Relaxed);
    let state_clone = state.clone();
    drop(state);
    let _ = app.emit("index:state", state_clone);
}

#[tauri::command]
fn get_available_drives() -> Vec<DriveInfo> {
    list_available_drives()
}

#[tauri::command]
async fn start_indexing(app: tauri::AppHandle, selected_drives: Vec<char>) -> Result<(), String> {
    if selected_drives.is_empty() {
        return Err("Выберите хотя бы один диск".to_string());
    }

    CANCEL_FLAG.store(false, Ordering::Relaxed);
    STATS_FILES.store(0, Ordering::Relaxed);
    STATS_DIRS.store(0, Ordering::Relaxed);
    STATS_SCANNED.store(0, Ordering::Relaxed);

    {
        let mut state = INDEX_STATE.write();
        *state = IndexState {
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
    emit_state(&app);

    tauri::async_runtime::spawn_blocking(move || {
        let start_time = Instant::now();

        let drive_results: Vec<_> = selected_drives
            .par_iter()
            .map(|&drive| {
                let app_clone = app.clone();

                let result = unsafe {
                    match MftScanner::open(drive) {
                        Ok(scanner) => {
                            {
                                let mut state = INDEX_STATE.write();
                                if let Some(d) = state.drives.iter_mut().find(|d| d.letter == drive)
                                {
                                    d.method = "MFT".to_string();
                                }
                            }
                            emit_state(&app_clone);

                            scanner.enumerate_mft().map(|raw| {
                                let entries = build_paths_topological(raw, drive);
                                entries
                                    .into_iter()
                                    .map(|(name, path, is_dir, is_hidden, modified)| {
                                        (name, path, is_dir, is_hidden, 0u64, modified)
                                    })
                                    .collect::<Vec<_>>()
                            })
                        }
                        Err(_) => {
                            {
                                let mut state = INDEX_STATE.write();
                                if let Some(d) = state.drives.iter_mut().find(|d| d.letter == drive)
                                {
                                    d.method = "Walker".to_string();
                                }
                            }
                            emit_state(&app_clone);
                            scan_drive_fallback(drive)
                        }
                    }
                };

                let count = result.as_ref().map(|v| v.len() as u64).unwrap_or(0);
                STATS_SCANNED.fetch_add(count, Ordering::Relaxed);

                {
                    let mut state = INDEX_STATE.write();
                    if let Some(d) = state.drives.iter_mut().find(|d| d.letter == drive) {
                        d.finished = true;
                        d.scanned = count;
                    }
                }
                emit_state(&app_clone);

                result
            })
            .collect();

        if CANCEL_FLAG.load(Ordering::Relaxed) {
            let mut state = INDEX_STATE.write();
            state.running = false;
            state.finished = true;
            state.elapsed_ms = start_time.elapsed().as_millis() as u64;
            emit_state(&app);
            return;
        }

        let total_entries: usize = drive_results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .map(|v| v.len())
            .sum();

        let mut builder = IndexBuilder::with_capacity(total_entries);

        for result in drive_results {
            if let Ok(entries) = result {
                for (name, path, is_dir, is_hidden, size, modified) in entries {
                    if is_dir {
                        STATS_DIRS.fetch_add(1, Ordering::Relaxed);
                    } else {
                        STATS_FILES.fetch_add(1, Ordering::Relaxed);
                    }
                    builder.add(&name, &path, is_dir, is_hidden, size, modified);
                }
            }
        }

        emit_state(&app);

        let index = builder.finalize();
        INDEX.store(Arc::new(Some(Arc::new(index))));

        {
            let mut state = INDEX_STATE.write();
            state.running = false;
            state.finished = true;
            state.elapsed_ms = start_time.elapsed().as_millis() as u64;
            state.total_files = STATS_FILES.load(Ordering::Relaxed);
            state.total_dirs = STATS_DIRS.load(Ordering::Relaxed);
            state.total_scanned = STATS_SCANNED.load(Ordering::Relaxed);
        }
        emit_state(&app);
    });

    Ok(())
}

#[tauri::command]
fn get_index_status() -> Result<IndexState, String> {
    Ok(INDEX_STATE.read().clone())
}

#[tauri::command]
async fn search(query: String, filters: FilterOptions) -> Vec<SearchResult> {
    let q = query.trim();
    if q.is_empty() {
        return vec![];
    }

    let index_guard = INDEX.load();
    let index = match index_guard.as_ref() {
        Some(idx) => idx.clone(),
        None => return vec![],
    };

    let q_lower = q.to_lowercase();
    let q_bytes = q_lower.as_bytes();
    let max_results = filters.max_results as usize;

    let type_filters: Vec<Vec<u8>> = filters
        .file_types
        .iter()
        .map(|ft| ft.trim().trim_start_matches('.').to_lowercase())
        .filter(|ft| !ft.is_empty())
        .map(|ft| ft.into_bytes())
        .collect();

    let has_type_filter = !type_filters.is_empty();

    let entry_count = index.entries.len();

    let mut results: Vec<(SearchResult, f32)> = (0..entry_count)
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

            let finder = Finder::new(q_bytes);
            let match_pos = finder.find(name_lower)?;

            if has_type_filter && !entry.is_dir() {
                let has_matching_ext = name_lower
                    .iter()
                    .rposition(|&b| b == b'.')
                    .map(|dot_pos| {
                        let ext = &name_lower[dot_pos + 1..];
                        type_filters.iter().any(|tf| tf.as_slice() == ext)
                    })
                    .unwrap_or(false);

                if !has_matching_ext {
                    return None;
                }
            }

            let name = index.entry_name(entry);
            let path = index.entry_path(entry);

            let modified_str = if entry.modified > 0 {
                let st = UNIX_EPOCH + Duration::from_secs(entry.modified);
                format_system_time(st).unwrap_or_default()
            } else {
                String::new()
            };

            let score = calculate_score(name_lower.len(), q_bytes.len(), match_pos);

            Some((
                SearchResult {
                    name: name.to_string(),
                    path: path.to_string(),
                    size: entry.size,
                    modified: modified_str,
                    is_dir: entry.is_dir(),
                    score: Some(score),
                },
                score,
            ))
        })
        .collect();

    results.par_sort_unstable_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.name.len().cmp(&b.0.name.len()))
    });

    results.truncate(max_results);
    results.into_iter().map(|(r, _)| r).collect()
}

#[tauri::command]
async fn open_file(path: String) -> Result<(), String> {
    use std::process::Command;
    Command::new("cmd")
        .args(["/C", "start", "", &path])
        .spawn()
        .map_err(|e| format!("Не удалось открыть файл: {}", e))?;
    Ok(())
}

#[tauri::command]
async fn open_folder(path: String) -> Result<(), String> {
    use std::process::Command;
    let folder_path = Path::new(&path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(path);
    Command::new("explorer")
        .arg(&folder_path)
        .spawn()
        .map_err(|e| format!("Не удалось открыть папку: {}", e))?;
    Ok(())
}

#[tauri::command]
async fn open_folder_and_select(path: String) -> Result<(), String> {
    use std::process::Command;
    Command::new("explorer")
        .args(["/select,", &path])
        .spawn()
        .map_err(|e| format!("Не удалось открыть: {}", e))?;
    Ok(())
}

#[tauri::command]
async fn delete_file(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if p.is_dir() {
        fs::remove_dir_all(p).map_err(|e| format!("Не удалось удалить папку: {}", e))?;
    } else {
        fs::remove_file(p).map_err(|e| format!("Не удалось удалить файл: {}", e))?;
    }
    Ok(())
}

#[tauri::command]
async fn rename_file(old_path: String, new_name: String) -> Result<String, String> {
    let old = Path::new(&old_path);
    let parent = old
        .parent()
        .ok_or("Невозможно получить родительскую папку")?;
    let new_path = parent.join(&new_name);
    fs::rename(old, &new_path).map_err(|e| format!("Не удалось переименовать: {}", e))?;
    Ok(new_path.to_string_lossy().to_string())
}

#[tauri::command]
async fn copy_path_to_clipboard(path: String) -> Result<(), String> {
    use std::process::Command;
    let powershell_cmd = format!("Set-Clipboard -Value '{}'", path.replace('\'', "''"));
    Command::new("powershell")
        .args(["-Command", &powershell_cmd])
        .output()
        .map_err(|e| format!("Не удалось скопировать путь: {}", e))?;
    Ok(())
}

#[tauri::command]
fn get_file_properties(path: String) -> Result<StdHashMap<String, String>, String> {
    let p = Path::new(&path);
    let meta = fs::metadata(p).map_err(|e| format!("Не удалось получить информацию: {}", e))?;

    let mut props = StdHashMap::new();
    props.insert("size".to_string(), meta.len().to_string());
    props.insert("is_dir".to_string(), meta.is_dir().to_string());
    props.insert("is_file".to_string(), meta.is_file().to_string());
    props.insert(
        "readonly".to_string(),
        meta.permissions().readonly().to_string(),
    );

    if let Ok(modified) = meta.modified() {
        if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
            props.insert("modified".to_string(), duration.as_secs().to_string());
        }
    }

    Ok(props)
}

#[inline(always)]
fn calculate_score(name_len: usize, query_len: usize, match_pos: usize) -> f32 {
    if name_len == query_len && match_pos == 0 {
        return 100.0;
    }
    if match_pos == 0 {
        return 90.0 + (10.0 * query_len as f32 / name_len as f32);
    }
    let pos_penalty = (match_pos as f32 * 2.0).min(30.0);
    let len_bonus = (query_len as f32 / name_len as f32) * 20.0;
    60.0 - pos_penalty + len_bonus
}

fn format_system_time(st: SystemTime) -> Option<String> {
    const FMT: &[FormatItem<'static>] =
        format_description!("[year]-[month]-[day] [hour]:[minute]");
    let dur = st.duration_since(UNIX_EPOCH).ok()?;
    let odt = time::OffsetDateTime::from_unix_timestamp(dur.as_secs() as i64).ok()?;
    Some(odt.format(FMT).ok()?)
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_available_drives,
            start_indexing,
            get_index_status,
            search,
            open_file,
            open_folder,
            open_folder_and_select,
            delete_file,
            rename_file,
            copy_path_to_clipboard,
            get_file_properties,
        ])
        .run(tauri::generate_context!())
        .expect("Failed to run application");
}