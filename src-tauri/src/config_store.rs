use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "config.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RedirectMode {
    List,
    All,
}

impl Default for RedirectMode {
    fn default() -> Self {
        RedirectMode::List
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub proxy_host: String,
    pub proxy_port: u16,
    pub proxy_username: String,
    pub router_port: u16,
    pub redirect_mode: RedirectMode,
    pub redirect_hosts: Vec<String>,
    pub main_host: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            proxy_host: String::new(),
            proxy_port: 1080,
            proxy_username: String::new(),
            router_port: 9090,
            redirect_mode: RedirectMode::List,
            redirect_hosts: vec![
                "music.youtube.com".to_string(),
                "youtubei.googleapis.com".to_string(),
            ],
            main_host: "https://music.youtube.com".to_string(),
        }
    }
}

pub fn load(app: &AppHandle) -> Result<AppConfig, String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    match store.get("config") {
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| e.to_string()),
        None => Ok(AppConfig::default()),
    }
}

pub fn save(app: &AppHandle, config: &AppConfig) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    let value = serde_json::to_value(config).map_err(|e| e.to_string())?;
    store.set("config".to_string(), value);
    store.save().map_err(|e| e.to_string())
}
