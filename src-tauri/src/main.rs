#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::{
    cmp::min,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState};
use memchr::memmem::Finder;
use regex::{bytes::Regex as BytesRegex, Regex, RegexBuilder};
use time::{
    format_description::FormatItem,
    macros::format_description,
    Date, PrimitiveDateTime, Time as TTime, UtcOffset,
};

// ——— Входные/выходные структуры ———

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FilterOptions {
    file_types: Vec<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    modified_from: Option<String>,
    modified_to: Option<String>,
    include_paths: Vec<String>,
    exclude_paths: Vec<String>,
    search_mode: String, // "substring" | "regex" | "fuzzy"
    case_sensitive: bool,
    search_in_content: bool,
    follow_symlinks: bool,
    include_hidden: bool,
    max_results: u32,
    // область поиска: "full" | "home" | "cwd" | "custom"
    search_scope: Option<String>,
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

// ——— Вспомогательные константы/типы ———

const MAX_CONTENT_BYTES: usize = 8 * 1024 * 1024;

// Тут был E0121: нужен полный тип для массива формат-элементов
const DATE_FMT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

#[derive(Copy, Clone)]
enum SearchMode {
    Substring,
    Regex,
    Fuzzy,
}

// ——— Основная команда ———

#[tauri::command]
async fn search(query: String, filters: FilterOptions) -> Vec<SearchResult> {
    let q = query.trim().to_string();
    if q.is_empty() {
        return vec![];
    }

    // Режим поиска как enum (надёжнее, чем сравнивать строки потом)
    let mode = match filters.search_mode.as_str() {
        "regex" => SearchMode::Regex,
        "fuzzy" => SearchMode::Fuzzy,
        _ => SearchMode::Substring,
    };
    let case_sensitive = filters.case_sensitive;

    // Готовим варианты запроса
    let q_norm_str = if case_sensitive { q.clone() } else { q.to_lowercase() };

    // Компилируем матчеры по имени
    let re_name = match mode {
        SearchMode::Regex => Some(
            RegexBuilder::new(&q)
                .case_insensitive(!case_sensitive)
                .build()
                .unwrap_or_else(|_| Regex::new("$^").unwrap()),
        ),
        _ => None,
    };

    // Матчеры по содержимому (опционально)
    let finder_cs = match mode {
        SearchMode::Substring => Some(Finder::new(q.as_bytes())),
        _ => None,
    };
    let finder_ci = match (mode, case_sensitive) {
        (SearchMode::Substring, false) => Some(Finder::new(q_norm_str.as_bytes())),
        _ => None,
    };
    let re_bytes = match (mode, filters.search_in_content) {
        (SearchMode::Regex, true) => Some(
            regex::bytes::RegexBuilder::new(&q)
                .case_insensitive(!case_sensitive)
                .build()
                .unwrap_or_else(|_| BytesRegex::new("$^").unwrap()),
        ),
        _ => None,
    };

    // Расширения
    let mut exts: Vec<String> = filters
        .file_types
        .iter()
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    exts.sort();
    exts.dedup();

    // Корни обхода: весь ПК по умолчанию
    let roots = roots_for_scope(&filters);

    // Исключения
    let (exclude_prefixes, globset) = build_excluders(&filters.exclude_paths);

    // Фильтр дат
    let from_time = parse_ymd_to_system_time(filters.modified_from.as_deref());
    let to_time = parse_ymd_to_system_time(filters.modified_to.as_deref())
        .map(|t| t + Duration::from_secs(24 * 3600 - 1));

    // Лимит
    let max_results = filters.max_results as usize;

    // Общие состояния
    let found = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<Vec<SearchResult>>> =
        Arc::new(Mutex::new(Vec::with_capacity(min(1024, max_results))));
    let local_offset = UtcOffset::current_local_offset().ok();

    // WalkBuilder без игноров (полное сканирование)
    let mut builder = WalkBuilder::new(&roots[0]);
    for r in roots.iter().skip(1) {
        builder.add(r);
    }
    builder
        .hidden(!filters.include_hidden)
        .follow_links(filters.follow_symlinks)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false);

    // Делимся данными между потоками через Arc
    let q_arc = Arc::new(q.clone()); // <- клонируем, чтобы не «переносить» q
    let q_norm_arc = Arc::new(q_norm_str.clone());
    let re_name = Arc::new(re_name);
    let re_bytes = Arc::new(re_bytes);
    let finder_cs = Arc::new(finder_cs);
    let finder_ci = Arc::new(finder_ci);
    let exts_arc = Arc::new(exts);
    let globset = Arc::new(globset);
    let exclude_prefixes = Arc::new(exclude_prefixes);

    let min_size = filters.min_size;
    let max_size = filters.max_size;
    let search_in_content = filters.search_in_content;

    builder.build_parallel().run(|| {
        // Эта фабрика вызывается для каждого воркера — внутри клонируем Arc
        let results = Arc::clone(&results);
        let found = Arc::clone(&found);

        let re_name = Arc::clone(&re_name);
        let re_bytes = Arc::clone(&re_bytes);
        let finder_cs = Arc::clone(&finder_cs);
        let finder_ci = Arc::clone(&finder_ci);
        let q = Arc::clone(&q_arc);
        let q_norm = Arc::clone(&q_norm_arc);
        let exts = Arc::clone(&exts_arc);
        let globset = Arc::clone(&globset);
        let exclude_prefixes = Arc::clone(&exclude_prefixes);

        Box::new(move |res| {
            if found.load(Ordering::Relaxed) >= max_results {
                return WalkState::Quit;
            }

            let entry = match res {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            let path = entry.path();

            // Исключения
            if is_excluded(path, &exclude_prefixes, globset.as_ref()) {
                return WalkState::Continue;
            }

            // Имя и тип
            let name_os = entry.file_name();
            let name = name_os.to_string_lossy();
            let is_dir = match entry.file_type() {
                Some(ft) => ft.is_dir(),
                None => path.is_dir(),
            };

            // Фильтр по расширению (только файлы)
            if !is_dir && !exts.is_empty() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if !exts.binary_search(&ext.to_lowercase()).is_ok() {
                        return WalkState::Continue;
                    }
                } else {
                    return WalkState::Continue;
                }
            }

            // Метаданные
            let meta = entry.metadata().ok();
            let (size, modified_opt, is_file) = match meta {
                Some(m) => {
                    let ft = m.file_type();
                    let is_file = ft.is_file();
                    let sz = if is_dir { 0 } else { m.len() };
                    let modified = m.modified().ok();
                    (sz, modified, is_file)
                }
                None => (0, None, false),
            };

            // Размеры
            if !is_dir {
                if let Some(min_s) = min_size {
                    if size < min_s {
                        return WalkState::Continue;
                    }
                }
                if let Some(max_s) = max_size {
                    if size > max_s {
                        return WalkState::Continue;
                    }
                }
            }

            // Даты
            if let Some(modified) = modified_opt {
                if let Some(from) = from_time {
                    if modified < from {
                        return WalkState::Continue;
                    }
                }
                if let Some(to) = to_time {
                    if modified > to {
                        return WalkState::Continue;
                    }
                }
            }

            // Матч по имени
            let mut matched = false;
            let mut score: Option<f32> = None;
            match mode {
                SearchMode::Substring => {
                    if case_sensitive {
                        matched = name.contains(q.as_str());
                    } else {
                        matched = name.to_lowercase().contains(q_norm.as_str());
                    }
                }
                SearchMode::Regex => {
                    if let Some(re) = re_name.as_ref() {
                        matched = re.is_match(&name);
                    }
                }
                SearchMode::Fuzzy => {
                    if let Some(s) = fuzzy_score(&name, q.as_str(), case_sensitive) {
                        matched = true;
                        score = Some(s);
                    }
                }
            }

            // Поиск в содержимом
            if !matched && search_in_content && is_file {
                if let Ok(mut f) = File::open(path) {
                    let cap = std::cmp::min(size as usize, MAX_CONTENT_BYTES);
                    let mut buf = Vec::with_capacity(cap);
                    let _ = f.take(cap as u64).read_to_end(&mut buf);
                    match mode {
                        SearchMode::Substring => {
                            if case_sensitive {
                                if let Some(finder) = finder_cs.as_ref() {
                                    matched = finder.find(&buf).is_some();
                                }
                            } else {
                                let lowered = match std::str::from_utf8(&buf) {
                                    Ok(s) => s.to_lowercase().into_bytes(),
                                    Err(_) => buf
                                        .iter()
                                        .map(|b| if b.is_ascii_uppercase() { b.to_ascii_lowercase() } else { *b })
                                        .collect(),
                                };
                                if let Some(finder) = finder_ci.as_ref() {
                                    matched = finder.find(&lowered).is_some();
                                }
                            }
                        }
                        SearchMode::Regex => {
                            if let Some(reb) = re_bytes.as_ref() {
                                matched = reb.is_match(&buf);
                            }
                        }
                        SearchMode::Fuzzy => { /* обычно не ищут по содержимому */ }
                    }
                }
            }

            if !matched {
                return WalkState::Continue;
            }

            let modified_str = modified_opt
                .and_then(|t| format_system_time_local(t, local_offset))
                .unwrap_or_default();

            let mut guard = results.lock().unwrap();
            guard.push(SearchResult {
                name: name.to_string(),
                path: path.to_string_lossy().to_string(),
                size,
                modified: modified_str,
                is_dir,
                score,
            });

            let n = found.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= max_results {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });

    let mut out = Arc::try_unwrap(results).unwrap().into_inner().unwrap();
    // Немного сортируем: папки выше, потом по score
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => b
            .score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal),
    });
    out
}

// ——— Утилиты ———

fn is_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

fn build_excluders(patterns: &[String]) -> (Vec<PathBuf>, GlobSet) {
    let mut prefixes = Vec::new();
    let mut gb = GlobSetBuilder::new();

    for raw in patterns {
        let p = raw.trim();
        if p.is_empty() {
            continue;
        }

        if !is_glob(p) && !p.contains(std::path::MAIN_SEPARATOR) {
            let g1 = format!("**/{p}/**");
            let g2 = format!("**/{p}");
            let _ = gb.add(GlobBuilder::new(&g1).build().unwrap());
            let _ = gb.add(GlobBuilder::new(&g2).build().unwrap());
            continue;
        }

        if is_glob(p) {
            let _ = gb.add(GlobBuilder::new(p).build().unwrap());
        } else {
            prefixes.push(PathBuf::from(p));
        }
    }

    let gs = gb.build().unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap());
    (prefixes, gs)
}

fn is_excluded(path: &Path, prefixes: &[PathBuf], gs: &GlobSet) -> bool {
    if prefixes.iter().any(|p| path.starts_with(p)) {
        return true;
    }
    gs.is_match(path)
}

fn parse_ymd_to_system_time(s: Option<&str>) -> Option<SystemTime> {
    let s = s?;
    let date = Date::parse(s, format_description!("[year]-[month]-[day]")).ok()?;
    let pdt = PrimitiveDateTime::new(date, TTime::MIDNIGHT);
    let odt = if let Ok(off) = UtcOffset::current_local_offset() {
        pdt.assume_offset(off)
    } else {
        pdt.assume_utc()
    };
    let ts = odt.unix_timestamp();
    if ts >= 0 {
        Some(UNIX_EPOCH + Duration::from_secs(ts as u64))
    } else {
        Some(UNIX_EPOCH - Duration::from_secs((-ts) as u64))
    }
}

fn format_system_time_local(st: SystemTime, off: Option<UtcOffset>) -> Option<String> {
    let dur = st.duration_since(UNIX_EPOCH).ok()?;
    let mut odt = time::OffsetDateTime::from_unix_timestamp(dur.as_secs() as i64).ok()?;
    if let Some(o) = off {
        odt = odt.to_offset(o);
    }
    Some(odt.format(DATE_FMT).ok()?)
}

// Фаззи по подпоследовательности
fn fuzzy_score(hay: &str, needle: &str, case_sensitive: bool) -> Option<f32> {
    if needle.is_empty() {
        return Some(0.0);
    }
    let (h, n) = if case_sensitive {
        (hay.to_string(), needle.to_string())
    } else {
        (hay.to_lowercase(), needle.to_lowercase())
    };
    let h_bytes = h.as_bytes();
    let n_bytes = n.as_bytes();

    let mut i = 0usize;
    let mut j = 0usize;
    let mut score = 0.0f32;

    while i < h_bytes.len() && j < n_bytes.len() {
        if h_bytes[i] == n_bytes[j] {
            if i == 0 || !is_alnum(h_bytes[i - 1]) {
                score += 1.5;
            } else {
                score += 1.0;
            }
            j += 1;
        }
        i += 1;
    }

    if j == n_bytes.len() {
        let len = h_bytes.len().max(1) as f32;
        Some(score / len)
    } else {
        None
    }
}

fn is_alnum(b: u8) -> bool {
    (b'A'..=b'Z').contains(&b) || (b'a'..=b'z').contains(&b) || (b'0'..=b'9').contains(&b)
}

// Область поиска (весь ПК — по умолчанию)
fn roots_for_scope(filters: &FilterOptions) -> Vec<PathBuf> {
    match filters.search_scope.as_deref() {
        Some("custom") => {
            let v: Vec<PathBuf> = filters
                .include_paths
                .iter()
                .map(PathBuf::from)
                .collect();
            if v.is_empty() { all_roots_by_os() } else { v }
        }
        Some("cwd") => std::env::current_dir().ok().into_iter().collect(),
        Some("home") => home_dir().into_iter().collect(),
        _ => all_roots_by_os(), // покрывает Some("full"), None и любые другие строки
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        dirs::home_dir()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

fn all_roots_by_os() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let mut roots = Vec::new();
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            let p = PathBuf::from(&drive);
            if p.exists() {
                roots.push(p);
            }
        }
        if roots.is_empty() {
            roots.push(PathBuf::from("C:\\"));
        }
        roots
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![PathBuf::from("/")]
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![search])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}