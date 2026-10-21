## SnapPin v0.1.0 — first public release

A Windows screen-capture, recording and clipboard toolkit with a local HTTP API, so a script — or an AI coding agent — can drive it.

Rust core (Windows APIs) + Tauri 2 shell + React overlay UI. No cloud, no account, no telemetry.

**In this release**

- **Capture** — region, full screen, or a single window. Window capture uses Windows Graphics Capture, so a window that is *fully covered* by other windows still comes out right.
- **Long screenshot** — scroll-and-stitch for tall pages.
- **Annotate** — rectangle, ellipse, arrow, pencil, text, numbered steps; undo/redo; two-level colour palette; the toolbar is never captured into the frame.
- **Pin** — keep an image floating above everything else.
- **Record** — region or window, GPU (NVENC) encode, system audio via WASAPI loopback, MP4/GIF output, pause/resume.
- **Clipboard history** — a `Win+V`-style panel for text and image entries, with search.
- **OCR** — fully local PP-OCRv5, Chinese-first, no round-trip to a cloud API. Every recognised line comes back with its coordinates.
- **Local HTTP API** — `127.0.0.1` with bearer-token auth, so screenshots, recordings, OCR and clipboard can be scripted.
- **Shell** — tray-resident, single instance, rebindable global hotkeys, a settings window that stays in one pane.

**Assets**

- `snappin-v0.1.0-windows-x64.zip` — the portable build. Unzip anywhere and run `snappin.exe`; no installer, no admin rights, no registry entries. It starts in the tray — right-click the tray icon for the menu.
- `snappin-ocr-engine-v0.1.0.zip` — the OCR engine (ONNX Runtime + PP-OCRv5 models). Unzip it so that `onnxruntime.dll` and the `.onnx` models end up in `engines/oar-ocr/` next to `snappin.exe`; if the executable lives somewhere read-only, use `%LOCALAPPDATA%\snappin\engines\oar-ocr\` instead, or point `SNAPPIN_OCR_ENGINE_DIR` at it.

**Requirements**

- Windows 10/11, x86-64
- `ffmpeg` on `PATH` (or `SNAPPIN_FFMPEG` pointing at it) — only needed for recording
- WebView2 runtime — preinstalled on Windows 11

**Build from source**

```bash
npm install
npm run tauri dev      # run it
npm run tauri build    # produce a Windows installer / portable exe
```

No account, no cloud, no telemetry — nothing you capture leaves your machine.
