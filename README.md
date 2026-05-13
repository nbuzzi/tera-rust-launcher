# Tera Rust Launcher

![Tera Launcher Interface](https://forum.ragezone.com/attachments/tera-png.264594/)

## Description
Tera Rust Launcher is a custom game launcher designed for Tera Online. It provides features such as automatic updates, game file verification, and multi-language support.

## Key Features
- User authentication
- Automatic game updates
- File integrity checks
- Multi-language support (English, French, Russian, German)
- Custom game path configuration
- Hash file generation for game files

## Technologies Used
- JavaScript (Tauri framework)
- HTML/CSS
- Anime.js for animations

## Quick Start
1. Clone the repository
2. Install dependencies
3. Run the launcher

## Note
This launcher is a custom solution and not officially associated with Tera Online or its publishers.

## Full Tutorial
For a comprehensive guide on how to set up and use this launcher, please refer to the full tutorial available at:

[Tera Rust Launcher Tutorial on Ragezone](https://forum.ragezone.com/threads/teralauncher-100-02-advanced-game-launcher-with-tauri-js.1231496/)

## Protocol & Internals (must‑read for maintainers)
The launcher implements the BHS launcher ↔ `TERA.exe` Win32 IPC protocol.
If you need to touch anything related to the server list, account name,
session ticket, game events or anti‑cheat notifications, read these docs
first — they capture knowledge that took months to recover:

- [`docs/LAUNCHER-CLIENT-PROTOCOL.md`](docs/LAUNCHER-CLIENT-PROTOCOL.md) —
  full message catalogue, wire formats, proto2 schema, sort criteria, UIPI
  filter requirements, and the empirically‑verified handshake order.
- [`docs/PROTOCOL-FIX-2026-05.md`](docs/PROTOCOL-FIX-2026-05.md) —
  postmortem of the "empty server list" bug, what was wrong, how we found
  it (with the `fake-tera` spy in the parent server repo), and how to keep
  the fix from regressing.

## Disclaimer
This project is for educational purposes only. Always respect the terms of service of the game and its publishers.
