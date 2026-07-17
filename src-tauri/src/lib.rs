mod commands;
mod config_store;
mod gear_overlay;
mod router;
mod secrets;

use router::config::RouterConfig;
use std::sync::Arc;
use tauri::{Manager, Url, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::watch;

pub struct AppState {
    pub router_config_tx: watch::Sender<Arc<RouterConfig>>,
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
            let router_config = commands::to_router_config(&config, password);
            let (tx, rx) = watch::channel(Arc::new(router_config));
            app.manage(AppState {
                router_config_tx: tx,
            });

            // A bad saved router_port (e.g. one that falls inside a Windows reserved
            // TCP port range — WSAEACCES / os error 10013) must never prevent the
            // window from opening: that's the only place the user can fix it. Fall
            // back to an OS-assigned ephemeral port rather than aborting startup.
            let configured_port = config.router_port;
            let listener = router::bind(configured_port).unwrap_or_else(|e| {
                log::error!(
                    "could not bind configured router port {configured_port}: {e}; \
                     falling back to an OS-assigned port so the app can still start. \
                     Fix the router port in settings to restore proxying — if this is \
                     \"access forbidden\" (os error 10013), that port likely falls \
                     inside a reserved TCP range (`netsh interface ipv4 show \
                     excludedportrange protocol=tcp` to check)."
                );
                router::bind(0).expect("binding an OS-assigned port should never fail")
            });
            let port = listener.local_addr()?.port();

            tauri::async_runtime::spawn(async move {
                if let Err(e) = router::serve(listener, rx).await {
                    log::error!("router failed to run: {e}");
                }
            });

            let proxy_url = Url::parse(&format!("http://127.0.0.1:{port}"))?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("config.html".into()))
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
