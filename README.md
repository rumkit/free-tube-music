# FreeTubeMusic

A Tauri desktop wrapper around [music.youtube.com](https://music.youtube.com) for
Windows. YouTube Music's web client geo-gates a handful of startup/config
requests, but media streaming itself isn't region-restricted — this app routes
only the geo-gated request(s) through a remote, credentialed SOCKS5 proxy,
while everything else (especially media) goes direct.

See [CLAUDE.md](./CLAUDE.md) for how the routing and config storage work.

## Prerequisites

- Node.js and npm
- Rust, **MSVC toolchain** (the GNU toolchain fails to link — Tauri/WebView2
  on Windows needs MSVC):
  ```
  rustup toolchain install stable-x86_64-pc-windows-msvc
  rustup override set stable-x86_64-pc-windows-msvc   # run inside src-tauri/
  ```
- [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/) runtime
  (preinstalled on current Windows; the Tauri installer will prompt for it if
  missing)
- The [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for
  Windows (Visual Studio C++ Build Tools)

Install JS dependencies once:

```sh
npm install
```

## Run (development)

```sh
npm run tauri dev
```

Builds the Rust backend, launches the app with hot reload on the frontend
(`src/`), and opens the config page. Backend changes trigger a rebuild;
frontend changes reload in place.

To check the Rust side alone without launching the app:

```sh
cd src-tauri
cargo check
cargo test        # run the router unit tests
```

## Build (production)

```sh
npm run tauri build
```

Produces a release binary and platform installers under
`src-tauri/target/release/bundle/`. On Windows this includes an MSI
(`msi/`) and an NSIS installer (`nsis/`).

To build the raw executable only, without bundling installers:

```sh
npm run tauri build -- --no-bundle
```

## Publish / distribute a release

This repo has no CI release pipeline configured — releases are built and
distributed manually:

1. Bump the version in `src-tauri/tauri.conf.json` (`version`) and
   `package.json` to match.
2. Run `npm run tauri build` to produce the signed-or-unsigned installers in
   `src-tauri/target/release/bundle/{msi,nsis}/`.
3. Attach the installer(s) to a GitHub release (or your distribution channel
   of choice) tagged with the matching version.

Unsigned Windows installers will trigger a SmartScreen warning on first run.
To avoid that, configure code signing under `bundle.windows.certificateThumbprint`
(or `signCommand`) in `src-tauri/tauri.conf.json` — see the
[Tauri Windows code signing guide](https://v2.tauri.app/distribute/sign/windows/)
— before building the release you intend to distribute.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
