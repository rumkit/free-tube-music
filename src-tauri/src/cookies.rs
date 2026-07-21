//! Session-cookie persistence for the WebView2 webview.
//!
//! WebView2 (unlike full Edge/Chrome or an installed PWA) does not persist
//! *session-scoped* cookies to disk — they're dropped every time the app
//! closes. Google/YouTube Music carry part of the logged-in session in those
//! cookies, so losing them bounces the user through `consent.google.com` on the
//! next launch even though the persistent login cookies survive. See the plan
//! in the repo and WebView2Feedback #3444 for background.
//!
//! We work around it exactly like CEF's old `persistent_session_cookies`:
//!
//! - **Backup** (`backup`): snapshot *every* cookie in the WebView2 cookie store
//!   (all domains) via `WebviewWindow::cookies()` and write it, DPAPI-encrypted,
//!   to `cookies.dat` in the app data dir. Best-effort — never blocks shutdown.
//! - **Restore** (`restore`): re-inject only the cookies that WebView2 actually
//!   drops — the *session* ones (no expiry). Persistent cookies are left to
//!   WebView2's own on-disk store, which expires them on their honest schedule,
//!   so we never resurrect or extend anything. Restored cookies keep
//!   `Expiration::Session`, so no expiry is ever fabricated.
//!
//! Both directions go through safe Tauri/wry APIs (`cookies()` / `set_cookie()`),
//! which call into `ICoreWebView2CookieManager` under the hood — no COM here.

use serde::{Deserialize, Serialize};
use tauri::webview::cookie::{Cookie, CookieBuilder, Expiration, SameSite};
use tauri::{Manager, WebviewWindow};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};

const COOKIE_FILE: &str = "cookies.dat";

/// A serializable snapshot of one cookie. Mirrors the attributes WebView2's
/// `CreateCookie` + setters round-trip; `session` records whether the cookie had
/// no expiry (the class we restore).
#[derive(Serialize, Deserialize)]
struct CookieRecord {
    name: String,
    value: String,
    domain: Option<String>,
    path: Option<String>,
    secure: bool,
    http_only: bool,
    /// "Lax" | "Strict" | "None", or `None` if unspecified.
    same_site: Option<String>,
    /// True when the cookie has no expiry (a session cookie).
    session: bool,
}

impl CookieRecord {
    fn from_cookie(c: &Cookie<'_>) -> Self {
        let session = !matches!(c.expires(), Some(Expiration::DateTime(_)));
        CookieRecord {
            name: c.name().to_string(),
            value: c.value().to_string(),
            domain: c.domain().map(str::to_string),
            path: c.path().map(str::to_string),
            secure: c.secure().unwrap_or(false),
            http_only: c.http_only().unwrap_or(false),
            same_site: c.same_site().map(|s| match s {
                SameSite::Strict => "Strict",
                SameSite::Lax => "Lax",
                SameSite::None => "None",
            }
            .to_string()),
            session,
        }
    }

    /// Rebuild a session cookie. Always `Expiration::Session`: wry only calls
    /// `SetExpires` when the cookie carries a max-age/expiry, so this leaves it a
    /// genuine session cookie in WebView2 and fabricates no expiry.
    fn into_session_cookie(self) -> Cookie<'static> {
        let mut builder = CookieBuilder::new(self.name, self.value)
            .secure(self.secure)
            .http_only(self.http_only)
            .expires(Expiration::Session);
        if let Some(domain) = self.domain {
            builder = builder.domain(domain);
        }
        if let Some(path) = self.path {
            builder = builder.path(path);
        }
        if let Some(same_site) = self.same_site {
            let same_site = match same_site.as_str() {
                "Strict" => SameSite::Strict,
                "None" => SameSite::None,
                _ => SameSite::Lax,
            };
            builder = builder.same_site(same_site);
        }
        builder.build()
    }
}

fn cookie_path(window: &WebviewWindow) -> Result<std::path::PathBuf, String> {
    let dir = window
        .app_handle()
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join(COOKIE_FILE))
}

/// Snapshot every cookie in the store and write the DPAPI-encrypted backup.
/// Best-effort: logs and returns on any failure so it can be called from the
/// window-close handler without blocking shutdown.
pub fn backup(window: &WebviewWindow) {
    let cookies = match window.cookies() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("cookie backup: reading cookies failed: {e}");
            return;
        }
    };
    let records: Vec<CookieRecord> = cookies.iter().map(CookieRecord::from_cookie).collect();

    let json = match serde_json::to_vec(&records) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("cookie backup: serializing failed: {e}");
            return;
        }
    };
    let encrypted = match protect(&json) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("cookie backup: DPAPI encrypt failed: {e}");
            return;
        }
    };
    match cookie_path(window) {
        Ok(path) => {
            if let Err(e) = std::fs::write(&path, encrypted) {
                log::warn!("cookie backup: writing {} failed: {e}", path.display());
            } else {
                log::debug!("cookie backup: wrote {} cookies", records.len());
            }
        }
        Err(e) => log::warn!("cookie backup: resolving path failed: {e}"),
    }
}

/// Re-inject the backed-up *session* cookies into WebView2. Call this after the
/// window is built but *before* navigating to the authenticated site, so the
/// first request carries the restored session. No-op if there's no backup.
pub fn restore(window: &WebviewWindow) {
    let path = match cookie_path(window) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("cookie restore: resolving path failed: {e}");
            return;
        }
    };
    let encrypted = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        // No backup yet (first run, or the user cleared it) — nothing to restore.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            log::warn!("cookie restore: reading {} failed: {e}", path.display());
            return;
        }
    };
    let json = match unprotect(&encrypted) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("cookie restore: DPAPI decrypt failed: {e}");
            return;
        }
    };
    let records: Vec<CookieRecord> = match serde_json::from_slice(&json) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("cookie restore: deserializing failed: {e}");
            return;
        }
    };

    let mut restored = 0usize;
    for record in records.into_iter().filter(|r| r.session) {
        let name = record.name.clone();
        if let Err(e) = window.set_cookie(record.into_session_cookie()) {
            log::warn!("cookie restore: setting {name} failed: {e}");
        } else {
            restored += 1;
        }
    }
    log::debug!("cookie restore: restored {restored} session cookies");
}

/// DPAPI-encrypt `data` bound to the current user account (same protection
/// Chromium/WebView2 give their own cookie store).
fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(&input, PCWSTR::null(), None, None, None, 0, &mut output)
            .map_err(|e| e.to_string())?;
        Ok(take_blob(output))
    }
}

fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output)
            .map_err(|e| e.to_string())?;
        Ok(take_blob(output))
    }
}

/// Copy a DPAPI output blob into an owned `Vec` and free the LocalAlloc buffer.
unsafe fn take_blob(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let bytes = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
    let _ = LocalFree(Some(HLOCAL(blob.pbData as *mut _)));
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_roundtrip_recovers_plaintext() {
        let plain = br#"[{"name":"YSC","value":"abc123"}]"#;
        let encrypted = protect(plain).expect("encrypt");
        assert_ne!(encrypted, plain, "ciphertext must not equal plaintext");
        let decrypted = unprotect(&encrypted).expect("decrypt");
        assert_eq!(decrypted, plain);
    }

    #[test]
    fn dpapi_rejects_tampered_ciphertext() {
        let mut encrypted = protect(b"secret-session").expect("encrypt");
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0xff;
        assert!(unprotect(&encrypted).is_err(), "tampered blob must not decrypt");
    }

    #[test]
    fn session_cookie_survives_record_roundtrip() {
        // A session cookie (no expiry) with the attributes YT/Google set.
        let cookie = CookieBuilder::new("YSC", "session-value")
            .domain("youtube.com")
            .path("/")
            .secure(true)
            .http_only(true)
            .same_site(SameSite::None)
            .expires(Expiration::Session)
            .build();

        let record = CookieRecord::from_cookie(&cookie);
        assert!(record.session, "no-expiry cookie must be recorded as session");

        let rebuilt = record.into_session_cookie();
        assert_eq!(rebuilt.name(), "YSC");
        assert_eq!(rebuilt.value(), "session-value");
        assert_eq!(rebuilt.domain(), Some("youtube.com"));
        assert_eq!(rebuilt.path(), Some("/"));
        assert_eq!(rebuilt.secure(), Some(true));
        assert_eq!(rebuilt.http_only(), Some(true));
        assert_eq!(rebuilt.same_site(), Some(SameSite::None));
        // Never fabricate an expiry: it stays a session cookie.
        assert!(matches!(rebuilt.expires(), Some(Expiration::Session)));
    }

    #[test]
    fn persistent_cookie_is_recorded_as_non_session() {
        let expires = Expiration::DateTime(
            tauri::webview::cookie::time::OffsetDateTime::now_utc() + std::time::Duration::from_secs(86_400),
        );
        let cookie = CookieBuilder::new("SID", "persistent")
            .expires(expires)
            .build();
        let record = CookieRecord::from_cookie(&cookie);
        assert!(
            !record.session,
            "cookie with a future expiry must not be treated as a session cookie"
        );
    }
}
