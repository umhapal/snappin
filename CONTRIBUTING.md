# Contributing to SnapPin

Thanks for looking at SnapPin. Issues and pull requests are both welcome.

## Before you start

- **Windows-only code paths.** The capture, recording, clipboard and UI Automation layers are Win32-specific. Please keep new platform-specific code behind the existing module boundaries (`src-tauri/src/*.rs`) rather than spreading `#ifdef`-style checks through the frontend.
- **No absolute machine paths.** The project must build on a machine that is not the author's. If a binary or model has to be found at runtime, resolve it in this order: an explicit environment variable → `PATH` → a path relative to the executable.
- **No secrets, no personal data.** Configuration, captures, recordings, clipboard history and OCR results are the user's data; they never belong in the repository.

## Development

```bash
npm install
npm run tauri dev
```

Useful commands:

```bash
npm run build                              # type-check + build the frontend
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo check --manifest-path src-tauri/Cargo.toml
```

## Style

- **Comments explain *why*, not *what*.** The code says what it does; a comment should explain a non-obvious constraint, a rejected alternative, or a platform quirk. A comment that restates the line below it is noise.
- When you reject an approach, leave a one-line note saying why. Future readers cannot see the alternatives you discarded.
- Keep HTTP responses in the shape `{ "ok": true, ... }` / `{ "ok": false, "error": "..." }`.
- UI text goes through the settings/theme files; avoid hard-coded colours in components.

## Pull requests

1. Open an issue first for anything larger than a bug fix, so the design can be agreed on before the work.
2. Keep a pull request focused on one change.
3. Describe **what you changed and how you verified it** (the command you ran, and what you observed). "Should work" is not a verification.
4. CI runs on `windows-latest`: formatting, `cargo check`, the frontend build, and a release Tauri build. All four must be green.

## Releasing

Releases are built by CI, not by hand. Push a `v*` tag and the `release` workflow
builds the portable exe, smoke-tests it (it must answer `/v1/health` within 30 s or
the release is aborted), and publishes the assets.

The OCR engine archive is a separate asset because the models and the ONNX Runtime
dll are not in this repository. `scripts/build-ocr-engine.mjs` fetches them from
their upstream sources and verifies every sha256 before packaging:

```bash
node scripts/build-ocr-engine.mjs --out release-assets
```

If you bump a model name in `src-tauri/src/ocr.rs`, update the matching sha256 in
that script — the values are the ones oar-ocr's own download registry enforces, so
a mismatch means the upstream artifact changed and the pin must be re-checked.


## Reporting bugs

Include the SnapPin version, the Windows version, and the steps to reproduce. If a capture or recording misbehaves, the app log (`%APPDATA%\com.snappin.app\*.log`) is the most useful artefact — please attach the relevant lines.
