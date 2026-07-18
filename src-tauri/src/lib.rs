mod commands;
mod config_store;
mod gear_overlay;
mod router;
mod secrets;

use router::config::RouterConfig;
use std::sync::{Arc, Mutex};
use tauri::{Manager, Url, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::watch;

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
        .plugin(tauri_plugin_log::Builder::default().build())
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
            let initial_url = if startup_warning.is_none() && is_configured {
                WebviewUrl::External(Url::parse(&config.main_host)?)
            } else {
                WebviewUrl::App("config.html".into())
            };

            WebviewWindowBuilder::new(app, "main", initial_url)
                .title("FreeTubeMusic")
                .inner_size(900.0, 700.0)
                .proxy_url(proxy_url)
                .initialization_script(gear_overlay::GEAR_OVERLAY_JS)
                .build()?;

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
