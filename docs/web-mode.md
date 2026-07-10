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

The first web-mode pass covers the core provider, proxy, settings, environment,
and common configuration commands. Native desktop-only features such as tray
menus, file dialogs, window controls, deep-link events, and automatic app restart
are intentionally degraded or unavailable in browser mode.
