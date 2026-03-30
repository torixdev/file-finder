# FileFinder

Fast desktop file search engine for Windows, built with Rust and React.

Standard file search APIs are slow on large drives. FileFinder reads the
NTFS Master File Table directly and builds a trigram index in memory —
making search across 1,000,000+ files nearly instant.

## How it works

- reads MFT directly via Windows DeviceIoControl — no slow directory traversal
- builds trigram index from all file names at startup
- parallel search across all CPU cores via rayon
- results delivered to React frontend through Tauri IPC in under 100ms

## Stack

- **Rust** — MFT reader, trigram index, search engine
- **React + TypeScript** — user interface
- **Tauri** — desktop shell, ~11 MB binary, <80 MB RAM

## School project

Школьный проект г. Краснодар школа 37