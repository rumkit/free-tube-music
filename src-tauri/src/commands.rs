use crate::config_store::{self, AppConfig, RedirectMode as StoreRedirectMode};
use crate::router::config::{RedirectMode as RouterRedirectMode, RouterConfig, SocksTarget};
use crate::secrets;
use crate::AppState;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use tauri::{AppHandle, Manager, State, Url};

#[derive(Serialize)]
pub struct SaveResult {
    pub restart_required: bool,
}

pub(crate) fn to_router_config(config: &AppConfig, password: String) -> RouterConfig {
    let mode = match config.redirect_mode {
        StoreRedirectMode::All => RouterRedirectMode::All,
        StoreRedirectMode::List => {
            RouterRedirectMode::List(config.redirect_hosts.iter().cloned().collect::<HashSet<_>>())
        }
    };

    let upstream = if config.proxy_host.is_empty() {
        None
    } else {
        Some(SocksTarget {
            host: config.proxy_host.clone(),
            port: config.proxy_port,
            username: config.proxy_username.clone(),
            password,
        })
    };

    RouterConfig { mode, upstream }
}

#[tauri::command]
pub fn load_config(app: AppHandle) -> Result<AppConfig, String> {
    config_store::load(&app)
}

#[tauri::command]
pub async fn save_config(
    app: AppHandle,
    state: State<'_, AppState>,
    config: AppConfig,
    password: Option<String>,
) -> Result<SaveResult, String> {
    let previous = config_store::load(&app)?;
    let restart_required = previous.router_port != config.router_port;

    if let Some(ref new_password) = password {
        if !new_password.is_empty() {
            secrets::set_password(new_password)?;
        }
    }

    let effective_password = password
        .filter(|p| !p.is_empty())
        .or_else(secrets::get_password)
        .unwrap_or_default();

    if config.proxy_host.is_empty() {
        return Err("Proxy host is required".to_string());
    }

    test_socks5_auth(&config.proxy_host, config.proxy_port, &config.proxy_username, &effective_password).await?;

    config_store::save(&app, &config)?;

    if !restart_required {
        let router_config = to_router_config(&config, effective_password);
        state
            .router_config_tx
            .send(Arc::new(router_config))
            .map_err(|e| e.to_string())?;
    }

    Ok(SaveResult { restart_required })
}

async fn test_socks5_auth(host: &str, port: u16, username: &str, password: &str) -> Result<(), String> {
    use tokio_socks::Error;

    // A harmless target used only to force the SOCKS5 auth handshake; the connect
    // itself is expected to fail past that point (nothing listens on port 1), but
    // that failure looks different from an auth/handshake failure and is what we
    // treat as "credentials are good."
    tokio_socks::tcp::Socks5Stream::connect_with_password(
        (host, port),
        ("127.0.0.1", 1),
        username,
        password,
    )
    .await
    .map(|_| ())
    .or_else(|e| match e {
        // Post-auth failures dialing the dummy target: auth succeeded.
        Error::HostUnreachable
        | Error::NetworkUnreachable
        | Error::ConnectionRefused
        | Error::TtlExpired
        | Error::GeneralSocksServerFailure
        | Error::ConnectionNotAllowedByRuleset => Ok(()),
        // Everything else — unreachable proxy, bad auth, protocol errors — is a
        // real failure the user needs to see before we save their credentials.
        other => Err(format!("SOCKS5 proxy check failed: {other}")),
    })
}

#[tauri::command]
pub fn show_config(app: AppHandle) -> Result<(), String> {
    // The config page is a bundled asset; its runtime URL scheme differs by
    // platform (tauri://localhost on macOS/Linux, https://tauri.localhost on
    // Windows), so navigate via JS relative location instead of hand-building it.
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    window
        .eval("window.location.href = 'config.html'")
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_and_launch(app: AppHandle) -> Result<(), String> {
    let config = config_store::load(&app)?;
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    let url = Url::parse(&config.main_host).map_err(|e| e.to_string())?;
    window.navigate(url).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn request_restart(app: AppHandle) {
    app.restart();
}
