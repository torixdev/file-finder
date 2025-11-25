#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![cfg(target_os = "windows")]

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap as StdHashMap,
    ffi::OsStr,
    fs,
    mem,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
    slice,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use arc_swap::ArcSwap;
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use ignore::WalkBuilder;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
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
        winbase::{FILE_FLAG_BACKUP_SEMANTICS, DRIVE_FIXED, DRIVE_REMOVABLE},
        winnt::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ},
    },
};

// ============ Windows API Constants ============

const FSCTL_ENUM_USN_DATA: DWORD = 0x000900b3;
const FSCTL_QUERY_USN_JOURNAL: DWORD = 0x000900f4;

// ============ Windows Structures ============

#[repr(C)]
#[derive(Copy, Clone)]
struct MftEnumDataV0 {
    start_file_reference_number: ULONGLONG,
    low_usn: LONGLONG,
    high_usn: LONGLONG,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
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
#[allow(dead_code)]
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

// ============ Data Structures ============

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

// ============ Compact Entry ============

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    name_off: u32,
    name_len: u16,
    path_off: u32,
    path_len: u32,
    size: u64,
    modified: u64,
    flags: u16,
}

impl Entry {
    #[inline]
    fn is_dir(&self) -> bool {
        (self.flags & 0x01) != 0
    }

    #[inline]
    fn is_hidden(&self) -> bool {
        (self.flags & 0x02) != 0
    }
}

// ============ Fast Trie with String Interning ============

#[derive(Default)]
struct TrieNode {
    children: HashMap<char, Box<TrieNode>>,
    entry_ids: Vec<u32>,
}

impl TrieNode {
    fn insert(&mut self, key: &str, id: u32) {
        let mut node = self;
        for ch in key.chars() {
            node = node.children.entry(ch).or_insert_with(|| Box::new(TrieNode::default()));
        }
        node.entry_ids.push(id);
    }

    fn search_prefix(&self, prefix: &str) -> HashSet<u32> {
        let mut node = self;
        for ch in prefix.chars() {
            match node.children.get(&ch) {
                Some(n) => node = n,
                None => return HashSet::default(),
            }
        }
        let mut results = HashSet::default();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            results.extend(&n.entry_ids);
            for child in n.children.values() {
                stack.push(child.as_ref());
            }
        }
        results
    }
}

// ============ Index ============

struct Index {
    entries: Vec<Entry>,
    arena: Vec<u8>,
    name_trie: TrieNode,
    extension_map: HashMap<String, Vec<u32>>,
}

impl Index {
    #[inline]
    fn str_at(&self, off: u32, len: usize) -> &str {
        unsafe { std::str::from_utf8_unchecked(&self.arena[off as usize..off as usize + len]) }
    }

    #[inline]
    fn entry_name(&self, e: &Entry) -> &str {
        self.str_at(e.name_off, e.name_len as usize)
    }

    #[inline]
    fn entry_path(&self, e: &Entry) -> &str {
        self.str_at(e.path_off, e.path_len as usize)
    }

    fn search_candidates(&self, query: &str) -> Vec<u32> {
        let q = query.to_lowercase();
        let mut candidates = self.name_trie.search_prefix(&q);

        if !q.contains('.') {
            if let Some(ids) = self.extension_map.get(&q) {
                candidates.extend(ids);
            }
        }

        let mut result: Vec<u32> = candidates.into_iter().collect();
        result.sort_unstable();
        result.dedup();
        result
    }
}

// ============ Thread-Safe Index Builder ============

struct FileEntry {
    name: String,
    path: String,
    is_dir: bool,
    is_hidden: bool,
    size: u64,
    modified: u64,
}

struct IndexBuilder {
    entries: Vec<Entry>,
    arena: Vec<u8>,
    name_trie: TrieNode,
    extension_map: HashMap<String, Vec<u32>>,
}

impl IndexBuilder {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(500_000),
            arena: Vec::with_capacity(50_000_000),
            name_trie: TrieNode::default(),
            extension_map: HashMap::default(),
        }
    }

    fn intern(&mut self, s: &str) -> (u32, usize) {
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(s.as_bytes());
        (off, s.len())
    }

    fn add_batch(&mut self, batch: Vec<FileEntry>) {
        for entry in batch {
            self.add_entry(&entry.name, &entry.path, entry.is_dir, entry.is_hidden, entry.size, entry.modified);
        }
    }

    fn add_entry(&mut self, name: &str, full_path: &str, is_dir: bool, is_hidden: bool, size: u64, modified: u64) {
        let id = self.entries.len() as u32;
        let (name_off, name_len_usize) = self.intern(name);
        let (path_off, path_len) = self.intern(full_path);
        let name_len = name_len_usize.min(u16::MAX as usize) as u16;

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
            path_off,
            path_len: path_len as u32,
            size,
            modified,
            flags,
        });

        let name_lower = name.to_lowercase();
        self.name_trie.insert(&name_lower, id);

        if !is_dir {
            if let Some(ext_start) = name.rfind('.') {
                let ext = name[ext_start + 1..].to_lowercase();
                if !ext.is_empty() {
                    self.extension_map.entry(ext).or_default().push(id);
                }
            }
        }
    }

    fn finalize(mut self) -> Index {
        for ids in self.extension_map.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }

        Index {
            entries: self.entries,
            arena: self.arena,
            name_trie: self.name_trie,
            extension_map: self.extension_map,
        }
    }
}

// ============ MFT Scanner ============

struct MftScanner {
    drive_letter: char,
    volume_handle: HANDLE,
}

impl MftScanner {
    unsafe fn open(drive_letter: char) -> Result<Self, String> {
        let volume_path = format!("\\\\.\\{}:", drive_letter);
        let wide: Vec<u16> = OsStr::new(&volume_path).encode_wide().chain(Some(0)).collect();

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

    unsafe fn enumerate_mft(&self, tx: Sender<Vec<FileEntry>>) -> Result<u64, String> {
        let journal_data = self.query_usn_journal()?;

        let mut enum_data = MftEnumDataV0 {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: journal_data.next_usn,
        };

        const BUFFER_SIZE: usize = 8 * 1024 * 1024;
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut total_count = 0u64;
        let mut path_map: HashMap<u64, String> = HashMap::default();
        path_map.insert(0x5000000000005, format!("{}:\\", self.drive_letter));

        let mut batch: Vec<FileEntry> = Vec::with_capacity(10000);

        loop {
            if CANCEL_FLAG.load(Ordering::Relaxed) {
                return Ok(total_count);
            }

            let mut bytes_returned: DWORD = 0;
            let result = DeviceIoControl(
                self.volume_handle,
                FSCTL_ENUM_USN_DATA,
                &mut enum_data as *mut _ as LPVOID,
                mem::size_of::<MftEnumDataV0>() as DWORD,
                buffer.as_mut_ptr() as LPVOID,
                BUFFER_SIZE as DWORD,
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

            while offset + mem::size_of::<UsnRecordV2>() <= bytes_returned as usize {
                let record_ptr = buffer.as_ptr().add(offset) as *const UsnRecordV2;
                let record = &*record_ptr;

                if record.record_length == 0 || record.record_length > 0x10000 {
                    break;
                }

                let filename_offset = offset + record.file_name_offset as usize;
                let filename_len = (record.file_name_length / 2) as usize;

                if filename_offset + filename_len * 2 <= bytes_returned as usize {
                    let filename_ptr = buffer.as_ptr().add(filename_offset) as *const u16;
                    let filename_slice = slice::from_raw_parts(filename_ptr, filename_len);
                    let filename = String::from_utf16_lossy(filename_slice);

                    if filename != "." && filename != ".." && !filename.starts_with("$") {
                        let is_dir = (record.file_attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                        let is_hidden = (record.file_attributes & FILE_ATTRIBUTE_HIDDEN) != 0;

                        let parent_path = path_map.get(&record.parent_file_reference_number).map(|s| s.as_str()).unwrap_or("");

                        let full_path = if parent_path.ends_with('\\') {
                            format!("{}{}", parent_path, filename)
                        } else if parent_path.is_empty() {
                            format!("{}:\\{}", self.drive_letter, filename)
                        } else {
                            format!("{}\\{}", parent_path, filename)
                        };

                        if is_dir {
                            path_map.insert(record.file_reference_number, full_path.clone());
                        }

                        let modified = {
                            let ft = *record.time_stamp.QuadPart() as u64;
                            if ft > 116444736000000000 {
                                (ft - 116444736000000000) / 10000000
                            } else {
                                0
                            }
                        };

                        batch.push(FileEntry {
                            name: filename,
                            path: full_path,
                            is_dir,
                            is_hidden,
                            size: 0,
                            modified,
                        });

                        total_count += 1;

                        if batch.len() >= 5000 {
                            let _ = tx.send(std::mem::replace(&mut batch, Vec::with_capacity(10000)));
                        }
                    }
                }

                enum_data.start_file_reference_number = record.file_reference_number;
                offset += record.record_length as usize;
            }
        }

        if !batch.is_empty() {
            let _ = tx.send(batch);
        }

        Ok(total_count)
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

// ============ Fallback Scanner ============

fn scan_drive_fallback(drive: char, tx: Sender<Vec<FileEntry>>) -> Result<u64, String> {
    let root = format!("{}:\\", drive);
    let mut count = 0u64;
    let mut batch: Vec<FileEntry> = Vec::with_capacity(5000);

    let walker = WalkBuilder::new(&root)
        .hidden(false)
        .follow_links(false)
        .threads(4)
        .build_parallel();

    let (entry_tx, entry_rx) = unbounded::<FileEntry>();

    walker.run(|| {
        let tx = entry_tx.clone();
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

                let (size, modified) = if let Ok(meta) = entry.metadata() {
                    let sz = if is_dir { 0 } else { meta.len() };
                    let mt = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    (sz, mt)
                } else {
                    (0, 0)
                };

                let _ = tx.send(FileEntry {
                    name,
                    path: full_path,
                    is_dir,
                    is_hidden,
                    size,
                    modified,
                });
            }

            ignore::WalkState::Continue
        })
    });

    drop(entry_tx);

    for entry in entry_rx {
        batch.push(entry);
        count += 1;

        if batch.len() >= 5000 {
            let _ = tx.send(std::mem::replace(&mut batch, Vec::with_capacity(5000)));
        }
    }

    if !batch.is_empty() {
        let _ = tx.send(batch);
    }

    Ok(count)
}

// ============ Global State ============

static INDEX: Lazy<ArcSwap<Option<Arc<Index>>>> = Lazy::new(|| ArcSwap::new(Arc::new(None)));
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
        let drive_type = GetDriveTypeW(wide.as_ptr());
        match drive_type {
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
    let state = INDEX_STATE.read().clone();
    let _ = app.emit("index:state", state);
}

// ============ Commands ============

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
        let (entry_tx, entry_rx) = unbounded::<Vec<FileEntry>>();

        let handles: Vec<_> = selected_drives
            .iter()
            .map(|&drive| {
                let tx = entry_tx.clone();
                let app_clone = app.clone();

                thread::spawn(move || {
                    let count = unsafe {
                        match MftScanner::open(drive) {
                            Ok(scanner) => {
                                {
                                    let mut state = INDEX_STATE.write();
                                    if let Some(d) = state.drives.iter_mut().find(|d| d.letter == drive) {
                                        d.method = "MFT".to_string();
                                    }
                                }
                                scanner.enumerate_mft(tx.clone())
                            }
                            Err(_) => {
                                {
                                    let mut state = INDEX_STATE.write();
                                    if let Some(d) = state.drives.iter_mut().find(|d| d.letter == drive) {
                                        d.method = "Walker".to_string();
                                    }
                                }
                                scan_drive_fallback(drive, tx.clone())
                            }
                        }
                    };

                    {
                        let mut state = INDEX_STATE.write();
                        if let Some(d) = state.drives.iter_mut().find(|d| d.letter == drive) {
                            d.finished = true;
                            if let Ok(c) = count {
                                d.scanned = c;
                            }
                        }
                    }
                    emit_state(&app_clone);

                    count
                })
            })
            .collect();

        drop(entry_tx);

        let mut builder = IndexBuilder::new();
        let mut last_emit = Instant::now();

        for batch in entry_rx {
            if CANCEL_FLAG.load(Ordering::Relaxed) {
                break;
            }

            let files_count = batch.iter().filter(|e| !e.is_dir).count() as u64;
            let dirs_count = batch.iter().filter(|e| e.is_dir).count() as u64;

            builder.add_batch(batch);

            {
                let mut state = INDEX_STATE.write();
                state.total_scanned = builder.entries.len() as u64;
                state.total_files += files_count;
                state.total_dirs += dirs_count;
            }

            if last_emit.elapsed() > Duration::from_millis(100) {
                emit_state(&app);
                last_emit = Instant::now();
            }
        }

        for handle in handles {
            let _ = handle.join();
        }

        if !CANCEL_FLAG.load(Ordering::Relaxed) {
            let index = builder.finalize();
            INDEX.store(Arc::new(Some(Arc::new(index))));
        }

        {
            let mut state = INDEX_STATE.write();
            state.running = false;
            state.finished = true;
            state.elapsed_ms = start_time.elapsed().as_millis() as u64;
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

    let candidates = index.search_candidates(q);
    let max_results = filters.max_results as usize;

    let mut results: Vec<SearchResult> = candidates
        .par_iter()
        .filter_map(|&id| {
            let entry = index.entries.get(id as usize)?;

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

            if !filters.file_types.is_empty() && !entry.is_dir() {
                let name = index.entry_name(entry);
                let ext = name.rfind('.').map(|i| &name[i + 1..]).unwrap_or("");
                let ext_lower = ext.to_lowercase();

                if !filters.file_types.iter().any(|ft| {
                    let ft_clean = ft.trim_start_matches('.');
                    ft_clean.eq_ignore_ascii_case(&ext_lower)
                }) {
                    return None;
                }
            }

            let name = index.entry_name(entry).to_string();
            let path = index.entry_path(entry).to_string();

            let q_lower = q.to_lowercase();
            let name_lower = name.to_lowercase();

            if !name_lower.contains(&q_lower) {
                return None;
            }

            let modified_str = if entry.modified > 0 {
                let st = UNIX_EPOCH + Duration::from_secs(entry.modified);
                format_system_time(st).unwrap_or_default()
            } else {
                String::new()
            };

            Some(SearchResult {
                name,
                path,
                size: entry.size,
                modified: modified_str,
                is_dir: entry.is_dir(),
                score: Some(calculate_score(&name_lower, &q_lower)),
            })
        })
        .collect();

    results.par_sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    results.truncate(max_results);
    results
}

// ============ File Operations Commands ============

#[tauri::command]
async fn open_file(path: String) -> Result<(), String> {
    use std::process::Command;

    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(&["/C", "start", "", &path])
            .spawn()
            .map_err(|e| format!("Не удалось открыть файл: {}", e))?;
    }

    Ok(())
}

#[tauri::command]
async fn open_folder(path: String) -> Result<(), String> {
    use std::process::Command;

    let folder_path = if let Some(parent) = Path::new(&path).parent() {
        parent.to_string_lossy().to_string()
    } else {
        path
    };

    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Не удалось открыть папку: {}", e))?;
    }

    Ok(())
}

#[tauri::command]
async fn open_folder_and_select(path: String) -> Result<(), String> {
    use std::process::Command;

    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .args(&["/select,", &path])
            .spawn()
            .map_err(|e| format!("Не удалось открыть: {}", e))?;
    }

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
    let parent = old.parent().ok_or("Невозможно получить родительскую папку")?;
    let new_path = parent.join(&new_name);

    fs::rename(old, &new_path).map_err(|e| format!("Не удалось переименовать: {}", e))?;

    Ok(new_path.to_string_lossy().to_string())
}

#[tauri::command]
async fn copy_path_to_clipboard(path: String) -> Result<(), String> {
    use std::process::Command;

    #[cfg(target_os = "windows")]
    {
        let powershell_cmd = format!("Set-Clipboard -Value '{}'", path.replace("'", "''"));
        Command::new("powershell")
            .args(&["-Command", &powershell_cmd])
            .output()
            .map_err(|e| format!("Не удалось скопировать путь: {}", e))?;
    }

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
    props.insert("readonly".to_string(), meta.permissions().readonly().to_string());

    if let Ok(modified) = meta.modified() {
        if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
            props.insert("modified".to_string(), duration.as_secs().to_string());
        }
    }

    Ok(props)
}

// ============ Utility Functions ============

fn calculate_score(text: &str, query: &str) -> f32 {
    if text == query {
        return 100.0;
    }
    if text.starts_with(query) {
        return 90.0;
    }
    let pos = text.find(query).unwrap_or(text.len());
    let score = 50.0 - (pos as f32 / text.len() as f32) * 30.0;
    score.max(10.0)
}

fn format_system_time(st: SystemTime) -> Option<String> {
    const FMT: &[FormatItem<'static>] = format_description!("[year]-[month]-[day] [hour]:[minute]");
    let dur = st.duration_since(UNIX_EPOCH).ok()?;
    let odt = time::OffsetDateTime::from_unix_timestamp(dur.as_secs() as i64).ok()?;
    Some(odt.format(FMT).ok()?)
}

// ============ Main ============

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