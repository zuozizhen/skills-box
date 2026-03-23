# skills-box

`skills-box` is a desktop dashboard for discovering, browsing, and managing AI skills installed across local/global directories.

Built with:

- Tauri 2 (Rust backend)
- React + TypeScript + Vite (frontend)

## Features

- Multi-source skill scanning (Claude/Codex/global/project paths and `skills list --json`)
- Auto-detect filesystem changes (no polling loop)
- Optional AI summaries with DeepSeek:
  - one-line summary
  - detailed summary
- Favorites + tray menu quick access
- Per-skill "resummarize" action
- Clipboard helpers for paths and commands

## AI Key

DeepSeek key is user-provided and stored locally at:

- `~/.opcsoskills/config.json`

The key is never hardcoded in source.

## Development

Requirements:

- Node.js 20+
- Rust stable
- Tauri system dependencies

Install and run:

```bash
npm ci
npm run tauri dev
```

Build:

```bash
npm run build
```

Backend check:

```bash
cd src-tauri
cargo check
```

## Open Source Readiness

- CI workflow included: `.github/workflows/ci.yml`
- Security policy included: `SECURITY.md`
- Basic secret scan in CI

## License

Add your preferred license file (`LICENSE`) before publishing.
