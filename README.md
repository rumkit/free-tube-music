# FreeTubeMusic

A Tauri desktop wrapper around [music.youtube.com](https://music.youtube.com) for
Windows with SOCKS5 proxy support.

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

Releases are built by GitHub Actions (`.github/workflows/release.yml`), which
triggers on any pushed tag matching `v*`. It runs `cargo test`, builds the app,
attests build provenance, and publishes a GitHub release with the NSIS
installer, the MSI and the bare `.exe` attached.

1. Bump the version in **all three** manifests, which must match:
   `src-tauri/tauri.conf.json` (this is the one the installers take their
   version from), `package.json`, and `src-tauri/Cargo.toml`. Run `cargo check`
   from `src-tauri/` afterwards so `Cargo.lock` picks the new version up.
2. Commit, then tag and push:

   ```sh
   git tag v0.1.2 && git push origin v0.1.2
   ```

3. Watch the run with `gh run watch`. The release is published **non-draft**,
   so a successful build goes public immediately.

To build installers locally without releasing, run `npm run tauri build` — the
output lands in `src-tauri/target/release/bundle/{msi,nsis}/`. The workflow can
also be started by hand with `workflow_dispatch`, which builds and uploads
artifacts but publishes no release.

Unsigned Windows installers will trigger a SmartScreen warning on first run.
To avoid that, configure code signing under `bundle.windows.certificateThumbprint`
(or `signCommand`) in `src-tauri/tauri.conf.json` — see the
[Tauri Windows code signing guide](https://v2.tauri.app/distribute/sign/windows/)
— before building the release you intend to distribute.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
