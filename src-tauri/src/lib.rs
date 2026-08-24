mod commands;
mod config_store;
mod gear_overlay;
mod router;
mod secrets;

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
/// `netlog_file`, when set, adds Chromium's own network event capture
/// (`--log-net-log`) at `IncludeSensitive` level — request/response headers and
/// cookie decisions, no payload bytes. This is the instrument for the session
/// investigation: the sign-out happens while the app is running, so a capture
/// left on overnight records the exact request on which the session died,
/// including whether YouTube's rotating tokens were ever refreshed. The file
/// contains live cookie values; it must be treated as a credential and deleted
/// after analysis.
fn chromium_args(port: u16, netlog_file: Option<&str>) -> String {
    let mut args = format!(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
         --autoplay-policy=no-user-gesture-required \
         --proxy-server=http://127.0.0.1:{port}"
    );
    if let Some(path) = netlog_file {
        // Quoted so a path with spaces can't split into stray arguments and
        // corrupt the block above (which would silently drop the proxy).
        args.push_str(&format!(
            " --log-net-log=\"{path}\" --net-log-capture-mode=IncludeSensitive"
        ));
    }
    args
}

/// Resolve the timestamped netlog path when `FTM_NETLOG` is set (to anything
/// but `0`). Timestamped, not fixed: each capture is evidence, and a relaunch
/// must never overwrite the file that recorded the failure. Returns `None` —
/// with the reason logged — rather than failing startup; the capture is a
/// diagnostic, never a prerequisite.
fn netlog_path(app: &tauri::App) -> Option<String> {
    let enabled = std::env::var("FTM_NETLOG").is_ok_and(|v| !v.is_empty() && v != "0");
    if !enabled {
        return None;
    }
    let dir = match app.path().app_log_dir() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("netlog: resolving the log dir failed ({e}); capture disabled");
            return None;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("netlog: creating {} failed ({e}); capture disabled", dir.display());
        return None;
    }
    let now = tauri::webview::cookie::time::OffsetDateTime::now_utc();
    let file = format!(
        "netlog-{:04}{:02}{:02}-{:02}{:02}{:02}.json",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    Some(dir.join(file).to_string_lossy().into_owned())
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
        // Info, explicitly: the router's `accounts.*` CONNECT line is the cheapest
        // permanent proof that the DBSC heartbeat is still alive, and it must
        // not depend on whatever the default filter happens to pass.
        //
        // KeepSome(5) with a 256 KB cap — ~1.25 MB bounded, enough history to
        // cover a night's run. Never go back to the default KeepOne: it *deletes*
        // the old file once it's over the limit, which is what destroyed a week
        // of sign-out evidence. Local timestamps so log lines can be matched
        // against when the user actually saw a sign-out popup.
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(5))
                .max_file_size(256 * 1024)
                .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
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

            let initial_url = if launch_main {
                WebviewUrl::External(Url::parse(&config.main_host)?)
            } else {
                WebviewUrl::App("config.html".into())
            };

            let user_agent = chrome_user_agent();
            let netlog = netlog_path(app);
            if let Some(p) = &netlog {
                log::info!(
                    "netlog: capture enabled at {p} — the file holds live cookie \
                     values; treat it as a credential and delete it after analysis"
                );
            }
            let browser_args = chromium_args(port, netlog.as_deref());
            WebviewWindowBuilder::new(app, "main", initial_url)
                .title("FreeTubeMusic")
                .inner_size(900.0, 700.0)
                .proxy_url(proxy_url)
                .additional_browser_args(&browser_args)
                .user_agent(&user_agent)
                .initialization_script(gear_overlay::GEAR_OVERLAY_JS)
                .build()?;
            log::info!("webview browser args: {browser_args}");

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::load_config,
            commands::save_config,
            commands::show_config,
            commands::apply_and_launch,
            commands::request_restart,
            commands::take_startup_warning,
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
        assert!(chromium_args(9090, None).contains("--proxy-server=http://127.0.0.1:9090"));
        // Specifically the port passed in, not the configured default: the
        // router falls back to an OS-assigned port when the configured one
        // can't be bound.
        assert!(chromium_args(51234, None).contains("--proxy-server=http://127.0.0.1:51234"));
    }

    /// wry only adds these when it builds the default block, which our override
    /// bypasses. Autoplay especially: losing it stops playback starting on its own.
    #[test]
    fn browser_args_keep_wrys_defaults() {
        let args = chromium_args(9090, None);
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
        let args = chromium_args(9090, None);
        assert_eq!(args.matches("--disable-features=").count(), 1, "{args}");
    }

    /// The capture must be strictly additive: absent unless requested, and when
    /// present it must not disturb the proxy flag the routing design depends on.
    #[test]
    fn netlog_args_are_appended_only_when_requested() {
        assert!(!chromium_args(9090, None).contains("--log-net-log"));

        let args = chromium_args(9090, Some(r"C:\logs dir\netlog.json"));
        assert!(args.contains(r#"--log-net-log="C:\logs dir\netlog.json""#), "{args}");
        assert!(args.contains("--net-log-capture-mode=IncludeSensitive"), "{args}");
        assert!(args.contains("--proxy-server=http://127.0.0.1:9090"), "{args}");
    }
}
