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
    // Proxy disabled: nothing is gated, so every host takes the direct-dial
    // path. Keeping mode=List(empty) rather than teaching the router about a
    // disabled state preserves its 502-on-gated-host-without-upstream as a
    // loud signal for genuine misconfiguration.
    if !config.proxy_enabled {
        return RouterConfig {
            mode: RouterRedirectMode::List(HashSet::new()),
            upstream: None,
        };
    }

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

    if config.proxy_enabled {
        if config.proxy_host.is_empty() {
            return Err("Proxy host is required".to_string());
        }

        test_socks5_auth(&config.proxy_host, config.proxy_port, &config.proxy_username, &effective_password).await?;
    }

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
    // Must be an absolute URL: this command can be invoked from remote content
    // (the gear overlay on music.youtube.com), where a relative JS location
    // assignment would resolve against the *current* page's origin instead of
    // the app's own — e.g. https://consent.youtube.com/config.html (404)
    // instead of the bundled asset.
    //
    // The app's own origin differs between dev and production. Under
    // `tauri dev` (no devUrl configured) the CLI serves ../src from its
    // built-in dev server and injects its URL as build.dev_url — and codegen
    // then embeds *zero* assets, so http://tauri.localhost serves nothing in
    // dev. In production builds dev_url is None, assets are embedded, and the
    // local origin is http://tauri.localhost (http, not https — https is
    // opt-in via .use_https_scheme(true), unset here; Windows-only app).
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    let base = if tauri::is_dev() {
        app.config().build.dev_url.clone()
    } else {
        None
    }
    .unwrap_or_else(|| Url::parse("http://tauri.localhost/").expect("static URL is valid"));
    let url = base.join("config.html").map_err(|e| e.to_string())?;
    window.navigate(url).map_err(|e| e.to_string())
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

#[tauri::command]
pub fn take_startup_warning(state: State<'_, AppState>) -> Option<String> {
    state.startup_warning.lock().unwrap().take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_proxy_maps_to_nothing_gated() {
        let config = AppConfig {
            proxy_enabled: false,
            proxy_host: "proxy.example.com".to_string(),
            redirect_mode: StoreRedirectMode::All,
            ..AppConfig::default()
        };

        let router_config = to_router_config(&config, "secret".to_string());

        assert!(router_config.upstream.is_none());
        match router_config.mode {
            RouterRedirectMode::List(hosts) => assert!(hosts.is_empty()),
            RouterRedirectMode::All => panic!("disabled proxy must not gate all traffic"),
        }
    }
}
