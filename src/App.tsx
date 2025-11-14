import React, { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./app.css";

type SearchMode = "substring" | "regex" | "fuzzy";
type SearchScope = "full" | "home" | "cwd" | "custom";

type FilterOptions = {
    fileTypes: string[];
    minSize?: number | null;
    maxSize?: number | null;
    modifiedFrom?: string | null;
    modifiedTo?: string | null;
    includePaths: string[];
    excludePaths: string[];
    searchMode: SearchMode;
    caseSensitive: boolean;
    searchInContent: boolean;
    followSymlinks: boolean;
    includeHidden: boolean;
    maxResults: number;
    searchScope: SearchScope;
};

type Settings = {
    indexingEnabled: boolean;
    indexLocation: string;
    parallelism: number;
    respectGitIgnore: boolean;
    cacheTtlHours: number;
};

type SearchResult = {
    name: string;
    path: string;
    size: number;
    modified: string;
    isDir: boolean;
    score?: number | null;
};

type IndexState = {
    running: boolean;
    roots: string[];
    rootIndex: number;         // 0-based
    currentRoot: string | null;
    scanned: number;           // всего путей
    files: number;
    dirs: number;
    currentPath: string | null;
    elapsedMs: number;
    finished?: boolean;
    cancelled?: boolean;
    error?: string | null;
};

const defaultFilters: FilterOptions = {
    fileTypes: [],
    minSize: null,
    maxSize: null,
    modifiedFrom: null,
    modifiedTo: null,
    includePaths: [],
    excludePaths: [],
    searchMode: "substring",
    caseSensitive: false,
    searchInContent: false,
    followSymlinks: false,
    includeHidden: true,
    maxResults: 5000,
    searchScope: "full",
};

const defaultSettings: Settings = {
    indexingEnabled: true,
    indexLocation: "",
    parallelism: navigator.hardwareConcurrency || 4,
    respectGitIgnore: true,
    cacheTtlHours: 24,
};

function formatBytes(bytes: number) {
    if (bytes <= 0) return "—";
    const k = 1024;
    const sizes = ["B", "KB", "MB", "GB", "TB"];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i]}`;
}

function extFromName(name: string): string {
    const idx = name.lastIndexOf(".");
    if (idx < 0) return "";
    return name.slice(idx + 1).toLowerCase();
}

function iconFor(result: SearchResult): { name: string; colorClass: string } {
    if (result.isDir) return { name: "folder", colorClass: "c-folder" };
    const ext = extFromName(result.name);
    if (["pdf"].includes(ext)) return { name: "picture_as_pdf", colorClass: "c-pdf" };
    if (["txt", "md", "log", "rtf"].includes(ext)) return { name: "description", colorClass: "c-text" };
    if (["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg"].includes(ext)) return { name: "image", colorClass: "c-image" };
    if (["mp4", "mkv", "mov", "avi", "webm"].includes(ext)) return { name: "movie", colorClass: "c-video" };
    if (["mp3", "wav", "flac", "ogg", "m4a"].includes(ext)) return { name: "audio_file", colorClass: "c-audio" };
    if (["zip", "rar", "7z", "tar", "gz", "bz2"].includes(ext)) return { name: "folder_zip", colorClass: "c-archive" };
    if (["json", "yaml", "yml", "toml", "csv"].includes(ext)) return { name: "table", colorClass: "c-data" };
    if (["rs", "ts", "tsx", "js", "jsx", "go", "py", "java", "kt", "c", "h", "cpp", "cs", "php", "rb", "swift"].includes(ext))
        return { name: "code", colorClass: "c-code" };
    return { name: "insert_drive_file", colorClass: "c-default" };
}

export default function App() {
    const [query, setQuery] = useState("");
    const [filters, setFilters] = useState<FilterOptions>(defaultFilters);
    const [settings, setSettings] = useState<Settings>(defaultSettings);

    const [isSearching, setIsSearching] = useState(false);
    const [elapsed, setElapsed] = useState(0);
    const [results, setResults] = useState<SearchResult[]>([]);
    const [error, setError] = useState<string | null>(null);

    const [showFilters, setShowFilters] = useState(false);
    const [showSettings, setShowSettings] = useState(false);

    // Индексация (состояние из backend через события)
    const [indexState, setIndexState] = useState<IndexState | null>(null);

    useEffect(() => {
        let unlisten: null | (() => void) = null;
        (async () => {
            try {
                unlisten = await listen<IndexState>("index:state", (e) => {
                    setIndexState(e.payload);
                });
                // запросим статус при монтировании
                const s = (await invoke("get_index_status")) as IndexState | null;
                if (s) setIndexState(s);
                // автозапуск индексации при включенных настройках
                if (settings.indexingEnabled) {
                    await startIndexing();
                }
            } catch (e) {
                console.warn("index event bind error", e);
            }
        })();
        return () => { if (unlisten) unlisten(); };
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    const statusText = useMemo(() => {
        if (isSearching) return "Поиск…";
        if (error) return "Ошибка";
        const t = elapsed > 0 ? ` за ${Math.max(1, Math.round(elapsed))} мс` : "";
        return `${results.length} результатов${t}`;
    }, [isSearching, results.length, error, elapsed]);

    async function handleSearch() {
        setIsSearching(true);
        setError(null);
        setResults([]);
        setElapsed(0);
        try {
            const payload = {
                ...filters,
                fileTypes: filters.fileTypes.map((x) => x.trim()).filter(Boolean),
                includePaths: filters.includePaths.map((x) => x.trim()).filter(Boolean),
                excludePaths: filters.excludePaths.map((x) => x.trim()).filter(Boolean),
                minSize: filters.minSize || null,
                maxSize: filters.maxSize || null,
                modifiedFrom: filters.modifiedFrom || null,
                modifiedTo: filters.modifiedTo || null,
            };
            const t0 = performance.now();
            const res = (await invoke("search", { query: query.trim(), filters: payload })) as SearchResult[];
            const t1 = performance.now();
            setElapsed(t1 - t0);
            setResults(res || []);
        } catch (e: any) {
            console.error(e);
            setError(e?.message || "Не удалось выполнить поиск");
        } finally {
            setIsSearching(false);
        }
    }

    function onEnter(e: React.KeyboardEvent<HTMLInputElement>) {
        if (e.key === "Enter") handleSearch();
    }
    function resetFilters() {
        setFilters(defaultFilters);
    }

    async function startIndexing() {
        try {
            await invoke("start_indexing", {
                filters: {
                    ...filters,
                    fileTypes: filters.fileTypes.map((x) => x.trim()).filter(Boolean),
                    includePaths: filters.includePaths.map((x) => x.trim()).filter(Boolean),
                    excludePaths: filters.excludePaths.map((x) => x.trim()).filter(Boolean),
                },
            });
        } catch (e) {
            console.error("start_indexing error", e);
        }
    }
    async function cancelIndexing() {
        try { await invoke("cancel_indexing"); } catch (e) { console.error(e); }
    }

    const roots = indexState?.roots || [];
    const rootIndex = indexState?.rootIndex ?? 0;
    const rootPercent = roots.length > 0 ? Math.round((rootIndex / roots.length) * 100) : 0;

    return (
        <div className="wrap">
            <header className="topbar">
                <div className="brand">
                    <span className="logo-dot" />
                    FileFinder
                </div>
                <div className="actions">
                    {indexState?.running ? (
                        <button className="btn" onClick={cancelIndexing}>
                            <span className="spinner" /> Сканируется: {indexState?.currentRoot || "—"}
                        </button>
                    ) : (
                        <button className="btn ghost" onClick={startIndexing}>
                            <span className="ms">data_usage</span> Сканировать диски
                        </button>
                    )}
                    <button className="btn ghost" onClick={() => setShowFilters((v) => !v)}>
                        <span className="ms">tune</span> Фильтры
                    </button>
                    <button className="btn ghost" onClick={() => setShowSettings(true)}>
                        <span className="ms">settings</span> Настройки
                    </button>
                </div>
            </header>

            <section className="search-panel">
                <div className="searchbar">
                    <span className="ms leading">search</span>
                    <input
                        placeholder="Поиск по имени файла…"
                        value={query}
                        onChange={(e) => setQuery(e.target.value)}
                        onKeyDown={onEnter}
                    />
                    <button className="btn primary" onClick={handleSearch} disabled={isSearching}>
                        Найти
                    </button>
                </div>

                {/* Баннер индексации */}
                {indexState?.running && (
                    <div className="index-banner">
                        <div className="index-row">
                            <div className="index-info">
                                <span className="ms leading">bolt</span>
                                <div>
                                    <div className="muted">Идёт сканирование дисков</div>
                                    <div><b>{indexState.currentRoot || "—"}</b></div>
                                </div>
                            </div>
                            <div className="drives">
                                {roots.map((r, i) => (
                                    <span key={r} className={`chip ${i < rootIndex ? "done" : ""} ${i === rootIndex ? "active" : ""}`}>
                    <span className="dot" /> {r}
                  </span>
                                ))}
                            </div>
                        </div>
                        <div className="progress">
                            <div className="bar" style={{ ["--value" as any]: `${rootPercent}%` }} />
                        </div>
                        <div className="index-row">
                            <div className="kv">
                                <span><b>{indexState.files.toLocaleString()}</b> файлов</span>
                                <span><b>{indexState.dirs.toLocaleString()}</b> папок</span>
                                <span className="muted" title={indexState.currentPath || ""}>
                  {indexState.currentPath || "…"}
                </span>
                            </div>
                            <div className="progress marquee" style={{ width: 220 }}>
                                <div className="bar" />
                            </div>
                        </div>
                    </div>
                )}
            </section>

            <section className="results">
                <div className="status">
                    <span className="muted">{statusText}</span>
                    {indexState?.finished && !indexState?.running && (
                        <span className="pill ok">Индекс готов</span>
                    )}
                    {indexState?.cancelled && (
                        <span className="pill warn">Индексация отменена</span>
                    )}
                    {error && <span className="error">⚠ {error}</span>}
                </div>

                <table>
                    <thead>
                    <tr>
                        <th style={{ width: 36 }}></th>
                        <th>Имя</th>
                        <th>Путь</th>
                        <th>Размер</th>
                        <th>Изменён</th>
                        <th>Тип</th>
                    </tr>
                    </thead>
                    <tbody>
                    {isSearching && Array.from({ length: 8 }).map((_, i) => (
                        <tr key={`skel-${i}`}>
                            <td><span className="skel" style={{ display: "inline-block", width: 26, height: 26 }} /></td>
                            <td><div className="skel" style={{ height: 16, width: "60%" }} /></td>
                            <td><div className="skel" style={{ height: 16, width: "80%" }} /></td>
                            <td><div className="skel" style={{ height: 16, width: 60 }} /></td>
                            <td><div className="skel" style={{ height: 16, width: 100 }} /></td>
                            <td><div className="skel" style={{ height: 16, width: 70 }} /></td>
                        </tr>
                    ))}

                    {results.length === 0 && !isSearching && !error && (
                        <tr>
                            <td colSpan={6} className="empty">Ничего не найдено</td>
                        </tr>
                    )}

                    {results.map((r, i) => {
                        const ic = iconFor(r);
                        return (
                            <tr key={i}>
                                <td className="icon-cell">
                                    <span className={`ms icon ${ic.colorClass}`}>{ic.name}</span>
                                </td>
                                <td className="name" title={r.name}>{r.name}</td>
                                <td className="path monospace" title={r.path}>{r.path}</td>
                                <td>{r.isDir ? "—" : formatBytes(r.size)}</td>
                                <td>{r.modified || "—"}</td>
                                <td>{r.isDir ? "Папка" : "Файл"}</td>
                            </tr>
                        );
                    })}
                    </tbody>
                </table>
            </section>

            {showSettings && (
                <SettingsModal
                    settings={settings}
                    onClose={() => setShowSettings(false)}
                    onSave={(s) => {
                        setSettings(s);
                        if (s.indexingEnabled && !indexState?.running) startIndexing();
                    }}
                />
            )}
        </div>
    );
}

function SettingsModal({
                           settings, onClose, onSave,
                       }: { settings: Settings; onClose: () => void; onSave: (s: Settings) => void; }) {
    const [local, setLocal] = useState<Settings>(settings);
    function save() { onSave(local); onClose(); }
    return (
        <div className="modal-backdrop" onClick={onClose}>
            <div className="modal" onClick={(e) => e.stopPropagation()}>
                <h3>Настройки</h3>

                <label className="rowH">
                    <input
                        type="checkbox"
                        checked={local.indexingEnabled}
                        onChange={(e) => setLocal((s) => ({ ...s, indexingEnabled: e.target.checked }))}
                    />
                    <span>Включить индексирование</span>
                </label>

                <label>
                    Каталог индекса
                    <input
                        placeholder="/var/tmp/filefinder-index"
                        value={local.indexLocation}
                        onChange={(e) => setLocal((s) => ({ ...s, indexLocation: e.target.value }))}
                    />
                </label>

                <div className="row">
                    <label>
                        Параллелизм
                        <input
                            type="number"
                            min={1}
                            value={local.parallelism}
                            onChange={(e) => setLocal((s) => ({ ...s, parallelism: Number(e.target.value || 1) }))}
                        />
                    </label>
                    <label>
                        TTL кэша (ч)
                        <input
                            type="number"
                            min={1}
                            value={local.cacheTtlHours}
                            onChange={(e) => setLocal((s) => ({ ...s, cacheTtlHours: Number(e.target.value || 1) }))}
                        />
                    </label>
                </div>

                <label className="rowH">
                    <input
                        type="checkbox"
                        checked={local.respectGitIgnore}
                        onChange={(e) => setLocal((s) => ({ ...s, respectGitIgnore: e.target.checked }))}
                    />
                    <span>Учитывать .gitignore</span>
                </label>

                <div className="actions">
                    <button className="btn" onClick={onClose}>Отмена</button>
                    <button className="btn primary" onClick={save}>Сохранить</button>
                </div>
            </div>
        </div>
    );
}