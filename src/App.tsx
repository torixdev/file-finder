import {useEffect, useState, useRef, JSX} from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Icons } from "./icons";
import "./app.css";

type FilterOptions = {
    fileTypes: string[];
    minSize?: number | null;
    maxSize?: number | null;
    includeHidden: boolean;
    maxResults: number;
};

type SearchResult = {
    name: string;
    path: string;
    size: number;
    modified: string;
    isDir: boolean;
    score?: number | null;
};

type DriveInfo = {
    letter: string;
    driveType: string;
    available: boolean;
};

type DriveProgress = {
    letter: string;
    scanned: number;
    finished: boolean;
    method: string;
};

type IndexState = {
    running: boolean;
    drives: DriveProgress[];
    totalScanned: number;
    totalFiles: number;
    totalDirs: number;
    elapsedMs: number;
    finished: boolean;
};

type ContextMenu = {
    x: number;
    y: number;
    item: SearchResult;
} | null;

function formatBytes(bytes: number) {
    if (bytes <= 0) return "—";
    const k = 1024;
    const sizes = ["B", "KB", "MB", "GB", "TB"];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i]}`;
}

function formatNumber(n: number) {
    return n.toLocaleString("ru-RU");
}

function iconFor(result: SearchResult): { icon: JSX.Element; colorClass: string } {
    if (result.isDir) return { icon: Icons.folder, colorClass: "c-folder" };
    const ext = result.name.split(".").pop()?.toLowerCase() || "";
    if (["pdf"].includes(ext)) return { icon: Icons.pdf, colorClass: "c-pdf" };
    if (["txt", "md", "log"].includes(ext)) return { icon: Icons.description, colorClass: "c-text" };
    if (["png", "jpg", "jpeg", "gif", "webp", "svg"].includes(ext)) return { icon: Icons.image, colorClass: "c-image" };
    if (["mp4", "mkv", "mov", "avi"].includes(ext)) return { icon: Icons.video, colorClass: "c-video" };
    if (["mp3", "wav", "flac", "ogg"].includes(ext)) return { icon: Icons.audio, colorClass: "c-audio" };
    if (["zip", "rar", "7z", "tar", "gz"].includes(ext)) return { icon: Icons.archive, colorClass: "c-archive" };
    if (["rs", "ts", "tsx", "js", "jsx", "py", "java", "cpp", "c", "go"].includes(ext)) return { icon: Icons.code, colorClass: "c-code" };
    return { icon: Icons.file, colorClass: "c-default" };
}

export default function App() {
    const [query, setQuery] = useState("");
    const [filters, setFilters] = useState<FilterOptions>({
        fileTypes: [],
        minSize: null,
        maxSize: null,
        includeHidden: false,
        maxResults: 1000,
    });

    const [availableDrives, setAvailableDrives] = useState<DriveInfo[]>([]);
    const [selectedDrives, setSelectedDrives] = useState<string[]>([]);
    const [showDriveSelector, setShowDriveSelector] = useState(false);

    const [indexState, setIndexState] = useState<IndexState | null>(null);
    const [isSearching, setIsSearching] = useState(false);
    const [results, setResults] = useState<SearchResult[]>([]);

    const [contextMenu, setContextMenu] = useState<ContextMenu>(null);
    const [renameItem, setRenameItem] = useState<SearchResult | null>(null);
    const [newName, setNewName] = useState("");

    const contextMenuRef = useRef<HTMLDivElement>(null);

    useEffect(() => {
        const handleContextMenu = (e: MouseEvent) => {
            const target = e.target as HTMLElement;
            if (!target.closest(".custom-context-menu")) {
                e.preventDefault();
            }
        };
        document.addEventListener("contextmenu", handleContextMenu);

        invoke<DriveInfo[]>("get_available_drives").then((drives) => {
            setAvailableDrives(drives);
            setSelectedDrives(drives.map((d) => d.letter));
            setShowDriveSelector(true);
        });

        const unlisten = listen<IndexState>("index:state", (event) => {
            setIndexState(event.payload);
        });

        return () => {
            document.removeEventListener("contextmenu", handleContextMenu);
            unlisten.then((fn) => fn());
        };
    }, []);

    useEffect(() => {
        const handleClick = () => setContextMenu(null);
        document.addEventListener("click", handleClick);
        return () => document.removeEventListener("click", handleClick);
    }, []);

    useEffect(() => {
        if (contextMenu && contextMenuRef.current) {
            const menu = contextMenuRef.current;
            const menuRect = menu.getBoundingClientRect();
            const windowWidth = window.innerWidth;
            const windowHeight = window.innerHeight;

            let adjustedX = contextMenu.x;
            let adjustedY = contextMenu.y;

            if (contextMenu.x + menuRect.width > windowWidth) {
                adjustedX = windowWidth - menuRect.width - 10;
            }

            if (contextMenu.y + menuRect.height > windowHeight) {
                adjustedY = contextMenu.y - menuRect.height;
                if (adjustedY < 0) {
                    adjustedY = 10;
                }
            }

            if (adjustedX < 0) {
                adjustedX = 10;
            }

            if (adjustedY < 0) {
                adjustedY = 10;
            }

            if (adjustedX !== contextMenu.x || adjustedY !== contextMenu.y) {
                menu.style.left = `${adjustedX}px`;
                menu.style.top = `${adjustedY}px`;
            }
        }
    }, [contextMenu]);

    async function handleStartIndexing() {
        if (selectedDrives.length === 0) {
            alert("Выберите хотя бы один диск!");
            return;
        }

        setShowDriveSelector(false);
        await invoke("start_indexing", { selectedDrives: selectedDrives.map((s) => s.charAt(0)) });
    }

    async function handleSearch() {
        setIsSearching(true);
        try {
            const res = await invoke<SearchResult[]>("search", {
                query: query.trim(),
                filters: {
                    ...filters,
                    fileTypes: filters.fileTypes.map((x) => x.trim()).filter(Boolean),
                },
            });
            setResults(res || []);
        } catch (e) {
            console.error(e);
        } finally {
            setIsSearching(false);
        }
    }

    const toggleDrive = (letter: string) => {
        setSelectedDrives((prev) => (prev.includes(letter) ? prev.filter((d) => d !== letter) : [...prev, letter]));
    };

    const handleRowClick = async (item: SearchResult) => {
        try {
            await invoke("open_file", { path: item.path });
        } catch (e) {
            console.error(e);
            alert(`Не удалось открыть: ${e}`);
        }
    };

    const handleRowContextMenu = (e: React.MouseEvent, item: SearchResult) => {
        e.preventDefault();
        e.stopPropagation();

        setContextMenu({
            x: e.clientX,
            y: e.clientY,
            item,
        });
    };

    const handleOpenFile = async () => {
        if (!contextMenu) return;
        try {
            await invoke("open_file", { path: contextMenu.item.path });
            setContextMenu(null);
        } catch (e) {
            alert(`Ошибка: ${e}`);
        }
    };

    const handleOpenFolder = async () => {
        if (!contextMenu) return;
        try {
            await invoke("open_folder", { path: contextMenu.item.path });
            setContextMenu(null);
        } catch (e) {
            alert(`Ошибка: ${e}`);
        }
    };

    const handleOpenFolderAndSelect = async () => {
        if (!contextMenu) return;
        try {
            await invoke("open_folder_and_select", { path: contextMenu.item.path });
            setContextMenu(null);
        } catch (e) {
            alert(`Ошибка: ${e}`);
        }
    };

    const handleDelete = async () => {
        if (!contextMenu) return;
        const confirmed = confirm(`Вы уверены, что хотите удалить "${contextMenu.item.name}"?`);
        if (!confirmed) return;

        try {
            await invoke("delete_file", { path: contextMenu.item.path });
            setResults((prev) => prev.filter((r) => r.path !== contextMenu.item.path));
            setContextMenu(null);
        } catch (e) {
            alert(`Ошибка удаления: ${e}`);
        }
    };

    const handleRenameStart = () => {
        if (!contextMenu) return;
        setRenameItem(contextMenu.item);
        setNewName(contextMenu.item.name);
        setContextMenu(null);
    };

    const handleRenameSubmit = async () => {
        if (!renameItem || !newName.trim()) return;

        try {
            const newPath = await invoke<string>("rename_file", {
                oldPath: renameItem.path,
                newName: newName.trim(),
            });

            setResults((prev) =>
                prev.map((r) =>
                    r.path === renameItem.path
                        ? {
                            ...r,
                            name: newName.trim(),
                            path: newPath,
                        }
                        : r
                )
            );
            setRenameItem(null);
            setNewName("");
        } catch (e) {
            alert(`Ошибка переименования: ${e}`);
        }
    };

    const handleCopyPath = async () => {
        if (!contextMenu) return;
        try {
            await invoke("copy_path_to_clipboard", { path: contextMenu.item.path });
            setContextMenu(null);
        } catch (e) {
            alert(`Ошибка копирования: ${e}`);
        }
    };

    const statusLine = indexState?.running
        ? `Сканирование ${indexState.drives.filter((d) => !d.finished).length} дисков — ${formatNumber(indexState.totalFiles)} файлов, ${formatNumber(indexState.totalDirs)} папок`
        : indexState?.finished
            ? `Готово за ${(indexState.elapsedMs / 1000).toFixed(1)}с — ${formatNumber(indexState.totalFiles)} файлов`
            : "Выберите диски для сканирования";

    return (
        <div className="wrap">
            {showDriveSelector && (
                <div className="modal-backdrop fade-in">
                    <div className="modal drive-selector slide-in">
                        <h2>Выберите диски для индексации</h2>
                        <div className="drive-list">
                            {availableDrives.map((drive, idx) => (
                                <label key={drive.letter} className="drive-item" style={{ animationDelay: `${idx * 0.05}s` }}>
                                    <input type="checkbox" checked={selectedDrives.includes(drive.letter)} onChange={() => toggleDrive(drive.letter)} />
                                    <span className="icon-wrapper">{Icons.storage}</span>
                                    <div className="drive-info">
                                        <strong>Диск {drive.letter}</strong>
                                        <span className="muted">{drive.driveType}</span>
                                    </div>
                                </label>
                            ))}
                        </div>
                        <div className="actions">
                            <button className="btn primary" onClick={handleStartIndexing} disabled={selectedDrives.length === 0}>
                                Начать сканирование ({selectedDrives.length})
                            </button>
                        </div>
                    </div>
                </div>
            )}

            <header className="topbar slide-down">
                <div className="brand">
                    <span className="logo-dot pulse" />
                    FileFinder
                </div>
                <div className="status-line">{statusLine}</div>
                {indexState?.finished && (
                    <button className="btn ghost" onClick={() => setShowDriveSelector(true)}>
                        <span className="icon-wrapper">{Icons.refresh}</span>
                        Пересканировать
                    </button>
                )}
            </header>

            {indexState?.running && (
                <section className="progress-panel fade-in">
                    {indexState.drives.map((drive, idx) => (
                        <div key={drive.letter} className={`drive-progress ${drive.finished ? "finished" : ""}`} style={{ animationDelay: `${idx * 0.1}s` }}>
                            <div className="drive-header">
                                <span className="icon-wrapper">{Icons.storage}</span>
                                <strong>Диск {drive.letter}</strong>
                                <span className="method">{drive.method}</span>
                            </div>
                            <div className="drive-stats">
                                {drive.finished ? (
                                    <span className="done">✓ {formatNumber(drive.scanned)}</span>
                                ) : (
                                    <span className="scanning">
                                        <span className="spinner-sm" />
                                        {formatNumber(drive.scanned)}
                                    </span>
                                )}
                            </div>
                        </div>
                    ))}
                </section>
            )}

            <section className="search-panel fade-in-up">
                <div className="searchbar">
                    <span className="icon-leading">{Icons.search}</span>
                    <input
                        type="text"
                        placeholder="Поиск файлов..."
                        value={query}
                        onChange={(e) => setQuery(e.target.value)}
                        onKeyDown={(e) => e.key === "Enter" && handleSearch()}
                        disabled={indexState?.running || !indexState?.finished}
                    />
                    <button className="btn primary" onClick={handleSearch} disabled={isSearching || indexState?.running || !indexState?.finished}>
                        {isSearching ? (
                            <>
                                <span className="spinner" /> Поиск...
                            </>
                        ) : (
                            "Найти"
                        )}
                    </button>
                </div>

                <div className="quick-filters">
                    <input
                        type="text"
                        placeholder="Фильтр по расширению (pdf, txt, rs)"
                        value={filters.fileTypes.join(", ")}
                        onChange={(e) => setFilters((f) => ({ ...f, fileTypes: e.target.value.split(",") }))}
                    />
                    <label>
                        <input type="checkbox" checked={filters.includeHidden} onChange={(e) => setFilters((f) => ({ ...f, includeHidden: e.target.checked }))} />
                        Показать скрытые
                    </label>
                </div>
            </section>

            <section className="results fade-in-up" style={{ animationDelay: "0.1s" }}>
                <div className="status">
                    <span className="muted">{results.length} результатов</span>
                </div>

                <table>
                    <thead>
                    <tr>
                        <th style={{ width: 36 }}></th>
                        <th>Имя</th>
                        <th>Путь</th>
                        <th>Размер</th>
                        <th>Изменён</th>
                    </tr>
                    </thead>
                    <tbody>
                    {results.length === 0 && !isSearching && (
                        <tr>
                            <td colSpan={5} className="empty">
                                {indexState?.finished ? "Ничего не найдено" : "Дождитесь завершения индексации"}
                            </td>
                        </tr>
                    )}
                    {results.map((r, i) => {
                        const ic = iconFor(r);
                        return (
                            <tr
                                key={i}
                                className="table-row-anim"
                                style={{ animationDelay: `${i * 0.02}s` }}
                                onClick={() => handleRowClick(r)}
                                onContextMenu={(e) => handleRowContextMenu(e, r)}
                            >
                                <td className="icon-cell">
                                    <span className={`icon ${ic.colorClass}`}>{ic.icon}</span>
                                </td>
                                <td className="name" title={r.name}>
                                    {r.name}
                                </td>
                                <td className="path monospace" title={r.path}>
                                    {r.path}
                                </td>
                                <td>{r.isDir ? "—" : formatBytes(r.size)}</td>
                                <td>{r.modified || "—"}</td>
                            </tr>
                        );
                    })}
                    </tbody>
                </table>
            </section>

            {contextMenu && (
                <div
                    ref={contextMenuRef}
                    className="custom-context-menu"
                    style={{
                        left: `${contextMenu.x}px`,
                        top: `${contextMenu.y}px`,
                    }}
                    onClick={(e) => e.stopPropagation()}
                >
                    <div className="context-item" onClick={handleOpenFile}>
                        <span className="icon-wrapper">{Icons.openInNew}</span>
                        Открыть
                    </div>
                    <div className="context-item" onClick={handleOpenFolderAndSelect}>
                        <span className="icon-wrapper">{Icons.folderOpen}</span>
                        Показать в папке
                    </div>
                    <div className="context-item" onClick={handleOpenFolder}>
                        <span className="icon-wrapper">{Icons.driveFileMove}</span>
                        Открыть папку
                    </div>
                    <div className="context-divider"></div>
                    <div className="context-item" onClick={handleRenameStart}>
                        <span className="icon-wrapper">{Icons.edit}</span>
                        Переименовать
                    </div>
                    <div className="context-item" onClick={handleCopyPath}>
                        <span className="icon-wrapper">{Icons.contentCopy}</span>
                        Копировать путь
                    </div>
                    <div className="context-divider"></div>
                    <div className="context-item danger" onClick={handleDelete}>
                        <span className="icon-wrapper">{Icons.delete}</span>
                        Удалить
                    </div>
                </div>
            )}

            {renameItem && (
                <div className="modal-backdrop fade-in" onClick={() => setRenameItem(null)}>
                    <div className="modal rename-modal slide-in" onClick={(e) => e.stopPropagation()}>
                        <h3>Переименовать</h3>
                        <input
                            type="text"
                            value={newName}
                            onChange={(e) => setNewName(e.target.value)}
                            onKeyDown={(e) => e.key === "Enter" && handleRenameSubmit()}
                            autoFocus
                        />
                        <div className="actions">
                            <button className="btn" onClick={() => setRenameItem(null)}>
                                Отмена
                            </button>
                            <button className="btn primary" onClick={handleRenameSubmit} disabled={!newName.trim()}>
                                Переименовать
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
}