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
            let window = WebviewWindowBuilder::new(app, "main", initial_url)
                .title("FreeTubeMusic")
                .inner_size(900.0, 700.0)
                .proxy_url(proxy_url)
                .user_agent(&user_agent)
                .initialization_script(gear_overlay::GEAR_OVERLAY_JS)
                .initialization_script(session_recovery::SESSION_RECOVERY_JS)
                .build()?;

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
