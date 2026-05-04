# FolderSync

A lightweight Tauri 2.0 desktop app that watches folders and mirrors changes (add / modify / delete) to a destination folder in real time.

## Features

- **Watch multiple folder pairs** simultaneously
- **Real-time sync**: files added or modified in the source are instantly copied to the destination
- **Deletions mirrored**: files removed from source are removed from destination
- **Initial sync** on pair creation: catches up the destination to match source
- **Pause / Resume** individual pairs without removing them
- **Activity log** with timestamped events (added / modified / removed / error)
- Minimal, system-tray-friendly footprint

## Prerequisites

- [Rust + Cargo](https://rustup.rs/) (stable toolchain)
- [Node.js](https://nodejs.org/) 18+
- Tauri v2 system dependencies — see [Tauri Prerequisites](https://tauri.app/start/prerequisites/)

### macOS extras
```
xcode-select --install
```

### Ubuntu/Debian extras
```
sudo apt install libwebkit2gtk-4.1-dev libssl-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev
```

### Windows
- Microsoft Visual Studio C++ Build Tools
- WebView2 (ships with Windows 10/11)

## Setup

```bash
# 1. Install JS dependencies
npm install

# 2. Run in dev mode (hot-reload UI, Rust auto-recompiles)
npm run dev

# 3. Build a release binary
npm run build
```

The distributable will be in `src-tauri/target/release/` (or `bundle/` for installers).

## Project Structure

```
folder-sync/
├── src/
│   └── index.html          # Vanilla HTML/CSS/JS frontend
├── src-tauri/
│   ├── src/
│   │   ├── main.rs         # Tauri entry point
│   │   └── lib.rs          # Sync engine + Tauri commands
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   └── build.rs
└── package.json
```

## How it works

1. When you add a sync pair, the Rust backend performs an **initial full sync** of source → destination.
2. A `notify` filesystem watcher is started on the source directory (recursive).
3. On `Create` / `Modify` events: the changed file/directory is copied to the mirrored path in destination.
4. On `Remove` events: the corresponding path in destination is deleted.
5. All events are emitted to the frontend via Tauri's event system and displayed in the activity log.

## Notes

- **Deletions are permanent** — there is no trash/undo. Use with care.
- The app does **not** sync from destination → source (one-way only).
- Large initial syncs of huge directories may take a moment.
