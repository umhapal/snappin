# Security Policy

## Supported versions

SnapPin is pre-1.0 and under active development. Fixes land on `main` and in the latest release.

## Reporting a vulnerability

Please report security issues privately through GitHub's "Report a vulnerability" button on the Security tab of the repository, rather than opening a public issue.

## Scope notes

SnapPin is a local screen-capture tool. Two properties are deliberate and should not be reported as bugs:

- **The HTTP API binds to `127.0.0.1` only.** It is not reachable from the network. Every endpoint except `/v1/health` requires the bearer token stored in `%APPDATA%\com.snappin.app\config.json`.
- **The app captures the screen and the clipboard by design.** Anything on screen, and anything you copy, is available to the local API and to the OCR engine (which runs locally, in-process).

Please do not bind the HTTP server to a public interface, and do not share `config.json` — it contains the API token.
