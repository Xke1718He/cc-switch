# CC Switch Web Mode

Web mode runs the existing Rust services as a local HTTP daemon and serves the
React UI in a normal browser. It is intended for Linux systems where the Tauri
WebKitGTK runtime is unavailable or unreliable.

## Development

Start the backend:

```bash
pnpm dev:web:server
```

Start the Vite frontend:

```bash
pnpm dev:web
```

Open <http://127.0.0.1:3000>. Vite proxies `/api/*` to the backend at
`127.0.0.1:31235`.

## Production

Build the browser UI:

```bash
pnpm build:web
```

Run the daemon:

```bash
cargo run --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --bin cc-switchd
```

The daemon serves the built UI at <http://127.0.0.1:31235>.

## Debian Package

Build a distributable headless Web package:

```bash
pnpm package:web:deb
```

The `.deb` is written to `src-tauri/target/packages/`. Install it with:

```bash
sudo apt install ./src-tauri/target/packages/cc-switch-web_*_*.deb
```

Run it as the current user:

```bash
cc-switch-web start
```

This starts the daemon in the background and opens/prints
<http://127.0.0.1:31235>. For foreground or service usage:

```bash
cc-switch-web server
systemctl --user enable --now cc-switch-web.service
```

Build the package on the oldest Linux version you want to support. For Ubuntu
20.04 compatibility, build on Ubuntu 20.04 or an equivalent container so the
resulting binary does not require a newer glibc/OpenSSL than the target system.

## Configuration

- `CC_SWITCH_WEB_HOST`: bind host, defaults to `127.0.0.1`
- `CC_SWITCH_WEB_PORT`: bind port, defaults to `31235`
- `CC_SWITCH_WEB_UI_DIR`: override the static UI directory, defaults to `dist/`

Keep the host bound to `127.0.0.1` unless an authenticated remote-access layer
is added. The API can read and write local AI-tool configuration and secrets.

## Current Scope

Web mode uses the current provider editor and route/Stack services, including
direct-provider state, proxy client addresses, and restoring route state at startup.
Session reading supports streamed transcript chunks, paged block content, image
bytes, and Markdown downloads in the browser. Connectivity checks use the same
fixed defaults as the desktop app.

Native desktop-only features such as tray
menus, file dialogs, window controls, deep-link events, and automatic app restart
are intentionally degraded or unavailable in browser mode.
Managed OAuth/Copilot sign-in flows remain unavailable in Web mode.

## Compatibility Checks

```bash
pnpm build:web
pnpm typecheck
pnpm exec vitest run tests/web/tauri-core.test.ts
cargo check --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --bin cc-switchd
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --test headless_web
```

The HTTP regression test launches a daemon with a temporary home and configuration
directory. It verifies provider editing, entering/exiting route mode, session
streaming, and paged content without modifying the user's configuration.
