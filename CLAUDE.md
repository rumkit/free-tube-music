# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Tauri v2 desktop wrapper around music.youtube.com for Windows. YouTube Music's
web client geo-gates a handful of startup/config requests, but media streaming
itself is not region-restricted. This app routes only the geo-gated request(s)
through a remote, credentialed SOCKS5 proxy, while everything else — especially
media — goes direct, to avoid unnecessary latency on the proxied hop.

Routing is done at the **host** level via a local CONNECT proxy the app runs
in-process, not via TLS interception — there is no certificate/MITM machinery
anywhere in this codebase, and none should be added for this purpose.

## Commands

Run from the repo root unless noted.

- `npm run tauri dev` — build and launch the app with hot reload.
- `npm run tauri build` — production build.
- `cargo check` (from `src-tauri/`) — fast compile check of the Rust backend.
- `cargo test` (from `src-tauri/`) — runs the router unit tests (`src-tauri/src/router/mod.rs`,
  `mod tests`). Run a single test with `cargo test <test_name>`, e.g.
  `cargo test non_gated_host_is_dialed_directly`.

**Toolchain:** this project is pinned to the MSVC Rust toolchain via
`rustup override set stable-x86_64-pc-windows-msvc` in `src-tauri/` — the GNU
toolchain fails to link (missing `dlltool.exe`). If `cargo` commands fail with
a dlltool error, check `rustup show` was run from inside `src-tauri/`.

## Architecture

### The router (`src-tauri/src/router/`)

An in-process tokio task, not a sidecar process — spawned once in `lib.rs`'s
`setup()` and lives for the app's lifetime. It's a plain HTTP CONNECT proxy on
`127.0.0.1:<router_port>` (default 9090):

- Reads the `CONNECT host:port` line (cleartext, no TLS termination needed).
- Looks up the current `RouterConfig` via a `tokio::sync::watch::Receiver` — a
  fresh snapshot is read per-connection, so config changes apply to new
  connections immediately without restarting the router.
- If the host matches the redirect list (or mode is "redirect all"), dials the
  target through the upstream SOCKS5 proxy via `tokio-socks`
  (`connect_with_password`, RFC 1929 auth). Otherwise dials the target directly.
- Splices the two streams with `tokio::io::copy_bidirectional`.

`router/config.rs` defines `RouterConfig` (decoupled from the persisted
`AppConfig` — see `commands::to_router_config` for the mapping) and
`SocksTarget`, which carries the upstream proxy's credentials in plaintext in
memory (read fresh from the keyring at config-build time, not cached
long-term).

The main window is created with `.proxy_url("http://127.0.0.1:<router_port>")`
(a Tauri/wry `WebviewWindowBuilder` method) so *all* WebView2 traffic flows
through this router — the router itself is what decides direct vs. proxied
per request, not the browser-level proxy config.

### Config & secrets split

- **Non-secret config** (`src-tauri/src/config_store.rs`, `AppConfig` struct):
  proxy host/port/username, router port, redirect mode + host list, main host.
  Persisted via `tauri-plugin-store` to `config.json` in the app data dir.
- **Secret** (`src-tauri/src/secrets.rs`): only the proxy password, stored via
  the `keyring` crate (Windows Credential Manager backend). It is never
  written to the store file and the config form never pre-fills it on reopen.

### Commands (`src-tauri/src/commands.rs`)

- `save_config` test-dials the SOCKS5 proxy (auth handshake against a
  throwaway target) before persisting anything, so bad credentials surface as
  an inline form error instead of being silently saved. See the `Error`
  variant match in `test_socks5_auth` — only post-auth connect failures
  (`HostUnreachable`, `ConnectionRefused`, etc.) are treated as "auth
  succeeded"; auth/handshake-level errors are surfaced to the user.
- Changing `router_port` requires a full app restart (it's baked into the
  webview's `proxy_url` at window-creation time and can't be mutated live) —
  `save_config` reports `restart_required` and the frontend calls
  `request_restart` (`AppHandle::restart()`) rather than hot-reloading.
  Every other config field hot-reloads via the router's watch channel.
- `show_config` / `apply_and_launch` navigate the single window in place
  (`window.eval("window.location.href = ...")` for the bundled config page,
  since its `tauri://` vs `https://tauri.localhost` scheme differs by
  platform; `window.navigate(Url)` for external URLs like `main_host`).

### Frontend (`src/`)

Plain HTML/JS, no framework, no build step — served directly as Tauri assets.
`config.html`/`config.js` is the only page; `src-tauri/src/gear_overlay.rs`
holds the JS injected via `initialization_script` on every page load
(including inside music.youtube.com) so there's always a way back to the
config page via a small fixed-position gear button.

## The session, and how it stays alive

The app's YouTube session is held by a **device-bound session** (DBSC) that the
WebView2 network stack manages entirely on its own — no page code, and nothing
in this repo. At interactive sign-in Google offers registration via
`secure-session-registration` headers; the stack POSTs
`accounts.google.com/RegisterSession` and thereafter refreshes
`.youtube.com`'s rotating `__Secure-*PSIDTS`/`*PSIDRTS` tokens by hitting
`accounts.youtube.com/RotateRelyingSession` every **483 s** (a 403 challenge
followed by a 200 signed retry). Without it, the session dies **2 h 01 m**
after it was minted.

Two consequences worth knowing before changing anything here:

- **`accounts.youtube.com` must keep working.** The router logs its CONNECT
  lines at `info!` precisely so this heartbeat is visible; a run whose log has
  no `router: connecting to accounts.youtube.com:443` lines at ~8 min intervals
  has a session that is already dying. Rotation is confirmed to work *through*
  the SOCKS proxy — device-bound sessions are not proxy-hostile.
- **If a daily sign-out ever recurs, the fix is very unlikely to be code.** The
  registration offer rides *interactive* sign-ins only, so an install whose
  sign-in predates a Google rollout never receives it, and silent re-auth can
  never repair that. One Google-account sign-out plus a password sign-in inside
  the app restores it. A 2026-08 investigation built several workarounds for
  this — cookie backup/restore, silent re-auth, tracking-prevention flags — and
  every one of them was later removed as useless. See the `stale/session-guard`
  branch for the full postmortem before writing anything similar.

`FTM_NETLOG=1` captures the app's own network stack to the logs dir, which is
the instrument that settled all of the above. It is inert unless the env var is
set. The Chrome user agent is also load-bearing: Google refuses interactive
sign-in without it, and interactive sign-in is the healing path.

## Known limitation to keep in mind

At runtime `redirect_mode` is `"all"` — every host goes through the SOCKS
proxy — so the default gated-host list (`music.youtube.com`,
`youtubei.googleapis.com` in `AppConfig::default()`) is not actually what's in
force, and remains an educated guess about which requests are geo-gated.

Host-level routing only works if the geo-gated request(s) and media-streaming
requests are served from *different* hosts — if that's not true, don't try to
fix it with finer-grained routing inside this router; that would require full
TLS termination (a real MITM layer), which is out of scope by design.

If narrowing `redirect_mode` to a list is ever attempted, note the untested
variable: today registration *and* rotation both egress from the proxy IP.
Selective routing would split them across two egress IPs, and whether Google
tolerates that mix has never been measured.
