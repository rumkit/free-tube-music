mod commands;
mod config_store;
mod cookies;
mod gear_overlay;
mod router;
mod secrets;
mod session_recovery;
mod tracking;

use router::config::RouterConfig;
use std::sync::{Arc, Mutex};
use tauri::{Manager, Url, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::watch;

/// A plain desktop-Chrome user agent. Google refuses interactive sign-in in
/// embedded webviews (it detects WebView2/`Edg` user agents as such), so we
/// present a stock Chrome UA instead. The major version is taken from the
/// installed WebView2 runtime so the string tracks the real engine and never
/// goes stale; if that lookup fails we fall back to a recent stable major.
fn chrome_user_agent() -> String {
    let major = tauri::webview_version()
        .ok()
        .and_then(|v| v.split('.').next().and_then(|m| m.parse::<u32>().ok()))
        .unwrap_or(138);
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    )
}

/// Command line handed to the WebView2 browser process.
///
/// **Setting this at all takes over wry's whole default argument block**
/// (`wry-0.55.1/src/webview2/mod.rs:294-322`): the `--proxy-server=` flag it
/// derives from `proxy_url` lives *inside* the `unwrap_or_else` fallback, as does
/// `--autoplay-policy`. So every one of those has to be reproduced here or it is
/// silently lost — and losing the proxy is the dangerous one: the app keeps
/// working, but every request goes direct and the geo-block comes back with no
/// error anywhere. `port` must be the port the router actually bound, which may
/// be an OS-assigned fallback rather than the configured one.
///
/// On top of the defaults, this disables Chromium's third-party-cookie phase-out
/// (`TrackingProtection3pcd`) and cross-site storage partitioning. That part is a
/// **trial**: the profile shows no sign third-party cookies are being blocked
/// (`cookie_controls_mode` unset, no cookie exceptions, and Edge already records
/// google.com/youtube.com as one organisation via `tracking_org_relationships`),
/// so this may well be a no-op. The log line at the call site records the exact
/// string used, so it's possible to tell afterwards what was actually in effect.
fn chromium_args(port: u16) -> String {
    format!(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,\
         TrackingProtection3pcd,ThirdPartyStoragePartitioning \
         --autoplay-policy=no-user-gesture-required \
         --proxy-server=http://127.0.0.1:{port}"
    )
}

pub struct AppState {
    pub router_config_tx: watch::Sender<Arc<RouterConfig>>,
    /// Set when the configured router port couldn't be bound at startup and a
    /// fallback ephemeral port is in use instead. Read (and cleared) once by
    /// the config page so the user sees why, then not shown again.
    pub startup_warning: Mutex<Option<String>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::default().build())
        // Info, explicitly: the cookie backup/restore lines are the only way to
        // tell whether the session actually survived a restart, and they must
        // not depend on whatever the default filter happens to pass.
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .setup(|app| {
            let handle = app.handle().clone();
            let config = config_store::load(&handle)?;
            let password = secrets::get_password().unwrap_or_default();
            // With the proxy disabled there's nothing to configure — launch
            // straight into main_host. Proxy-on still needs host + password.
            let is_configured = !config.proxy_enabled
                || (!config.proxy_host.is_empty() && !password.is_empty());
            let router_config = commands::to_router_config(&config, password);
            let (tx, rx) = watch::channel(Arc::new(router_config));

            // A bad saved router_port (e.g. one that falls inside a Windows reserved
            // TCP port range — WSAEACCES / os error 10013, or a port some other
            // process already occupies) must never prevent the window from opening:
            // that's the only place the user can fix it. Fall back to an OS-assigned
            // ephemeral port rather than aborting startup, and surface why on the
            // config page instead of silently proxying nothing.
            let configured_port = config.router_port;
            let mut startup_warning = None;
            let listener = router::bind(configured_port).unwrap_or_else(|e| {
                let message = format!(
                    "Couldn't start the router on port {configured_port} ({e}). \
                     Using a temporary port instead — pick a different port below \
                     and save to fix this permanently. If this says \"access \
                     forbidden\", that port is likely reserved by Windows or already \
                     in use by another program."
                );
                log::error!("{message}");
                startup_warning = Some(message);
                router::bind(0).expect("binding an OS-assigned port should never fail")
            });
            let port = listener.local_addr()?.port();

            app.manage(AppState {
                router_config_tx: tx,
                startup_warning: Mutex::new(startup_warning.clone()),
            });

            tauri::async_runtime::spawn(async move {
                if let Err(e) = router::serve(listener, rx).await {
                    log::error!("router failed to run: {e}");
                }
            });

            let proxy_url = Url::parse(&format!("http://127.0.0.1:{port}"))?;

            // Skip the setup page and go straight to YT Music once proxy
            // credentials are already saved — but only if the router actually
            // came up on the port the user configured; otherwise stay on the
            // config page so the startup_warning above is visible.
            let launch_main = startup_warning.is_none() && is_configured;

            // When launching straight into YT Music, hold on about:blank first so
            // no authenticated request fires before we've restored the session
            // cookies; then navigate. Otherwise show the config page as before.
            let initial_url = if launch_main {
                WebviewUrl::External(Url::parse("about:blank")?)
            } else {
                WebviewUrl::App("config.html".into())
            };

            let user_agent = chrome_user_agent();
            let browser_args = chromium_args(port);
            let window = WebviewWindowBuilder::new(app, "main", initial_url)
                .title("FreeTubeMusic")
                .inner_size(900.0, 700.0)
                .proxy_url(proxy_url)
                .additional_browser_args(&browser_args)
                .user_agent(&user_agent)
                .initialization_script(gear_overlay::GEAR_OVERLAY_JS)
                .initialization_script(session_recovery::SESSION_RECOVERY_JS)
                .build()?;
            log::info!("webview browser args: {browser_args}");

            // Allow Google's cross-site cookies to flow (WebView2 blocks
            // third-party cookies by default via tracking prevention), so the
            // logged-in session survives mid-use instead of being signed out.
            // Set before the first navigation below.
            tracking::disable(&window);

            if launch_main {
                // Re-inject session cookies WebView2 dropped on last close, then
                // navigate. restore() runs synchronously on the main thread (the
                // wry cookie message is handled inline here), so it completes
                // before navigation issues the first request.
                cookies::restore(&window);
                window.navigate(Url::parse(&config.main_host)?)?;
            }

            // Snapshot the cookie store periodically while the webview is alive,
            // and flush the newest snapshot on close. Best-effort; failures are
            // logged, never fatal.
            //
            // The close handler deliberately does *not* read cookies: that read
            // pumps the Windows message loop, which re-delivers the close event
            // and re-enters this handler, and WebView2 drops cookies as it tears
            // down — so a close-time read wrote a degraded snapshot over the good
            // one. See the `cookies` module docs.
            {
                let w = window.clone();
                let flushed = std::sync::atomic::AtomicBool::new(false);
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { .. } = event {
                        if !flushed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                            cookies::write_last(&w);
                        }
                    }
                });
            }
            {
                let w = window.clone();
                tauri::async_runtime::spawn(async move {
                    // Take the first snapshot early: the close handler can only
                    // flush a snapshot that already exists, so without this a
                    // session shorter than the interval would contribute nothing
                    // at all. 15s is long enough for the page to have loaded and
                    // settled its cookies.
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    cookies::backup(&w);

                    // Then 60s rather than the old 300s: this is now the only
                    // path that reads cookies, so it bounds how much of the
                    // session a crash — or a close between ticks — can cost.
                    let mut interval =
                        tokio::time::interval(std::time::Duration::from_secs(60));
                    interval.tick().await; // fires immediately; skip it
                    loop {
                        interval.tick().await;
                        cookies::backup(&w);
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::load_config,
            commands::save_config,
            commands::show_config,
            commands::apply_and_launch,
            commands::request_restart,
            commands::take_startup_warning,
            commands::log_page_event,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::chromium_args;

    /// The whole routing design depends on this flag. Because setting custom
    /// browser args replaces wry's defaults wholesale, dropping it wouldn't fail
    /// anywhere — the app would just quietly stop using the proxy.
    #[test]
    fn browser_args_carry_the_router_proxy_on_the_bound_port() {
        assert!(chromium_args(9090).contains("--proxy-server=http://127.0.0.1:9090"));
        // Specifically the port passed in, not the configured default: the
        // router falls back to an OS-assigned port when the configured one
        // can't be bound.
        assert!(chromium_args(51234).contains("--proxy-server=http://127.0.0.1:51234"));
    }

    /// wry only adds these when it builds the default block, which our override
    /// bypasses. Autoplay especially: losing it stops playback starting on its own.
    #[test]
    fn browser_args_keep_wrys_defaults() {
        let args = chromium_args(9090);
        for expected in [
            "msWebOOUI",
            "msPdfOOUI",
            "msSmartScreenProtection",
            "--autoplay-policy=no-user-gesture-required",
        ] {
            assert!(args.contains(expected), "{expected} missing from {args}");
        }
    }

    /// Chromium takes one --disable-features; a second would override the first.
    #[test]
    fn browser_args_pass_a_single_disable_features_switch() {
        let args = chromium_args(9090);
        assert_eq!(args.matches("--disable-features=").count(), 1, "{args}");
        assert!(args.contains("TrackingProtection3pcd"));
        assert!(args.contains("ThirdPartyStoragePartitioning"));
    }
}
