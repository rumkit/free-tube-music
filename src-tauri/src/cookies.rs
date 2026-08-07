//! Cookie persistence for the WebView2 webview.
//!
//! WebView2 (unlike full Edge/Chrome or an installed PWA) does not carry the
//! whole cookie jar across restarts. Concretely, the profile's on-disk store
//! ends up missing YouTube's *first-party* auth set (`SID`, `HSID`, `SSID`,
//! `APISID`, `SAPISID`, `__Secure-1PSID`, `LOGIN_INFO`, …) on `.youtube.com`,
//! while the equivalent `.google.com` set persists fine. That's why the app
//! comes up signed out but clicking "Sign in" completes with no password: the
//! Google master session is intact, only YouTube's own jar is gone.
//!
//! We work around it exactly like CEF's old `persistent_session_cookies`:
//!
//! - **Snapshot/backup** (`backup`): snapshot *every* cookie in the WebView2
//!   cookie store (all domains) via `WebviewWindow::cookies()`, keep it in
//!   memory, and write it DPAPI-encrypted to `cookies.dat` in the app data dir.
//! - **Restore** (`restore`): re-inject only the cookies that are **missing**
//!   from the live store at startup, and only if they haven't expired. Anything
//!   WebView2 kept itself is left strictly alone — re-injecting a superseded
//!   rotating token (`__Secure-3PSIDTS`, `__Secure-3PSIDRTS`) over a fresher one
//!   is exactly the replay pattern that invalidates a Google session. Expiries
//!   round-trip verbatim, so no expiry is ever fabricated or extended.
//!
//! Both directions go through safe Tauri/wry APIs (`cookies()` / `set_cookie()`),
//! which call into `ICoreWebView2CookieManager` under the hood — no COM here.
//!
//! ## Why the close path never reads cookies
//!
//! `window.cookies()` bottoms out at `webview2_com::wait_with_pump`, which pumps
//! the Windows message loop. Calling it from the `CloseRequested` handler
//! re-delivers the close message and re-enters the handler — the logs showed
//! ~35 nested backups during one close, and the snapshot degraded from 70 to 60
//! cookies mid-storm as WebView2 tore down, so the *degraded* snapshot is what
//! landed on disk. Hence the split: reads happen on the periodic task while the
//! webview is fully alive, and `write_last` at close only flushes the snapshot
//! already held in memory.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::webview::cookie::time::OffsetDateTime;
use tauri::webview::cookie::{Cookie, CookieBuilder, Expiration, SameSite};
use tauri::{Manager, WebviewWindow};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};

const COOKIE_FILE: &str = "cookies.dat";

/// Newest snapshot taken while the webview was alive. `write_last` flushes this
/// at close instead of taking a fresh (and by then degraded) read.
static LAST_SNAPSHOT: Mutex<Option<Vec<CookieRecord>>> = Mutex::new(None);

/// The full per-cookie inventory is logged once per run rather than on every
/// 60s tick — enough to diagnose what persists and what doesn't, without
/// writing ~70 lines a minute into the log file.
static INVENTORY_LOGGED: AtomicBool = AtomicBool::new(false);

/// A serializable snapshot of one cookie. Mirrors the attributes WebView2's
/// `CreateCookie` + setters round-trip.
#[derive(Serialize, Deserialize, Clone)]
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
    /// Unix timestamp at which the cookie expires; `None` for a session cookie.
    /// `serde(default)` so `cookies.dat` files written before this field existed
    /// stay readable (they simply restore as session cookies).
    #[serde(default)]
    expires: Option<i64>,
}

/// Identity of a cookie in the store, as WebView2 scopes it.
type CookieKey = (String, String, String);

impl CookieRecord {
    fn from_cookie(c: &Cookie<'_>) -> Self {
        let expires = match c.expires() {
            Some(Expiration::DateTime(dt)) => Some(dt.unix_timestamp()),
            _ => None,
        };
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
            session: expires.is_none(),
            expires,
        }
    }

    fn key(&self) -> CookieKey {
        (
            self.domain.clone().unwrap_or_default(),
            self.path.clone().unwrap_or_default(),
            self.name.clone(),
        )
    }

    /// Rebuild the cookie with its original lifetime. A persistent cookie keeps
    /// its exact expiry; a session cookie stays `Expiration::Session` (wry only
    /// calls `SetExpires` when the cookie carries one), so nothing is ever
    /// fabricated or extended.
    fn into_cookie(self) -> Cookie<'static> {
        let expiration = match self.expires.and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok())
        {
            Some(dt) => Expiration::DateTime(dt),
            None => Expiration::Session,
        };
        let mut builder = CookieBuilder::new(self.name, self.value)
            .secure(self.secure)
            .http_only(self.http_only)
            .expires(expiration);
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

/// Pick the backed-up cookies worth re-injecting: those the live store no longer
/// has, minus any that have already expired.
///
/// Kept free of Tauri types so it's unit-testable without a live webview.
fn to_restore<'a>(
    records: &'a [CookieRecord],
    present: &HashSet<CookieKey>,
    now: i64,
) -> Vec<&'a CookieRecord> {
    records
        .iter()
        .filter(|r| !present.contains(&r.key()))
        // A backup routinely contains short-TTL rotating tokens (`.google.com`'s
        // `__Secure-3PSIDRTS` lives ~10 minutes) that are long dead by the next
        // launch. Re-injecting one is at best a no-op.
        .filter(|r| r.expires.is_none_or(|ts| ts > now))
        .collect()
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

/// Read every cookie in the store. Must only be called while the webview is
/// alive — never from the close path (see the module docs).
fn snapshot(window: &WebviewWindow) -> Option<Vec<CookieRecord>> {
    let cookies = match window.cookies() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("cookie snapshot: reading cookies failed: {e}");
            return None;
        }
    };
    let records: Vec<CookieRecord> = cookies.iter().map(CookieRecord::from_cookie).collect();

    // Once per run: the full inventory, so it's possible to tell which cookies
    // WebView2 actually persists and which are lost between launches. Names and
    // domains only — cookie values are credentials and never logged.
    if !INVENTORY_LOGGED.swap(true, Ordering::Relaxed) {
        log::info!("cookie inventory: {} cookies in the live store", records.len());
        for r in &records {
            log::info!(
                "cookie inventory: {} {} session={} expires={}",
                r.domain.as_deref().unwrap_or("-"),
                r.name,
                r.session,
                r.expires.map_or("-".to_string(), |ts| ts.to_string()),
            );
        }
    }
    Some(records)
}

/// Encrypt and write a snapshot to `cookies.dat`.
fn write(records: &[CookieRecord], window: &WebviewWindow, context: &str) {
    let json = match serde_json::to_vec(records) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("cookie backup ({context}): serializing failed: {e}");
            return;
        }
    };
    let encrypted = match protect(&json) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("cookie backup ({context}): DPAPI encrypt failed: {e}");
            return;
        }
    };
    match cookie_path(window) {
        Ok(path) => {
            if let Err(e) = std::fs::write(&path, encrypted) {
                log::warn!(
                    "cookie backup ({context}): writing {} failed: {e}",
                    path.display()
                );
            } else {
                log::info!("cookie backup ({context}): wrote {} cookies", records.len());
            }
        }
        Err(e) => log::warn!("cookie backup ({context}): resolving path failed: {e}"),
    }
}

/// Snapshot the store, remember it, and write the encrypted backup. Called from
/// the periodic task only — it reads cookies, so it must not run at close time.
pub fn backup(window: &WebviewWindow) {
    let Some(records) = snapshot(window) else {
        return;
    };
    write(&records, window, "periodic");
    if let Ok(mut last) = LAST_SNAPSHOT.lock() {
        *last = Some(records);
    }
}

/// Flush the newest in-memory snapshot. Safe to call from the `CloseRequested`
/// handler: it takes no cookie read, so it never pumps the message loop and
/// can't re-enter the close handler.
pub fn write_last(window: &WebviewWindow) {
    let records = match LAST_SNAPSHOT.lock() {
        Ok(last) => last.clone(),
        Err(e) => {
            log::warn!("cookie backup (close): snapshot lock poisoned: {e}");
            return;
        }
    };
    match records {
        Some(records) => write(&records, window, "close"),
        // Nothing snapshotted yet (closed within the first tick). The backup
        // already on disk is fresher than anything we could write here.
        None => log::info!("cookie backup (close): no snapshot yet, keeping the existing file"),
    }
}

/// Re-inject backed-up cookies that the live store is missing. Call this after
/// the window is built but *before* navigating to the authenticated site, so the
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

    // What the live store already has. If this read fails we fall back to an
    // empty set, which restores everything non-expired — the pre-existing
    // behaviour, and better than restoring nothing.
    let present: HashSet<CookieKey> = match window.cookies() {
        Ok(live) => {
            log::info!("cookie restore: live store has {} cookies", live.len());
            live.iter().map(|c| CookieRecord::from_cookie(c).key()).collect()
        }
        Err(e) => {
            log::warn!("cookie restore: reading the live store failed ({e}); restoring all");
            HashSet::new()
        }
    };

    let now = OffsetDateTime::now_utc().unix_timestamp();
    let wanted = to_restore(&records, &present, now);
    log::info!(
        "cookie restore: {} of {} backed-up cookies are missing from the live store",
        wanted.len(),
        records.len()
    );

    let mut restored = 0usize;
    for record in wanted {
        let (domain, _, name) = record.key();
        if let Err(e) = window.set_cookie(record.clone().into_cookie()) {
            log::warn!("cookie restore: setting {domain} {name} failed: {e}");
        } else {
            log::info!("cookie restore: restored {domain} {name}");
            restored += 1;
        }
    }
    log::info!("cookie restore: restored {restored} cookies");
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

    /// A persistent `.youtube.com` auth cookie, the class this module exists for.
    fn persistent_record(name: &str, expires: i64) -> CookieRecord {
        CookieRecord {
            name: name.to_string(),
            value: "v".to_string(),
            domain: Some(".youtube.com".to_string()),
            path: Some("/".to_string()),
            secure: true,
            http_only: true,
            same_site: None,
            session: false,
            expires: Some(expires),
        }
    }

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
        assert_eq!(record.expires, None);

        let rebuilt = record.into_cookie();
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
    fn persistent_cookie_keeps_its_exact_expiry_through_a_roundtrip() {
        let expires = OffsetDateTime::now_utc() + std::time::Duration::from_secs(86_400);
        let cookie = CookieBuilder::new("SID", "persistent")
            .domain(".youtube.com")
            .expires(Expiration::DateTime(expires))
            .build();

        let record = CookieRecord::from_cookie(&cookie);
        assert!(
            !record.session,
            "cookie with a future expiry must not be treated as a session cookie"
        );
        assert_eq!(record.expires, Some(expires.unix_timestamp()));

        // The whole point of the fix: it must come back persistent, with the
        // same expiry, not downgraded to a session cookie.
        let rebuilt = record.into_cookie();
        match rebuilt.expires() {
            Some(Expiration::DateTime(dt)) => {
                assert_eq!(dt.unix_timestamp(), expires.unix_timestamp())
            }
            other => panic!("expected a persistent expiry, got {other:?}"),
        }
    }

    #[test]
    fn old_backup_without_expires_field_still_deserializes() {
        let json = br#"[{"name":"YSC","value":"v","domain":".youtube.com","path":"/",
            "secure":true,"http_only":true,"same_site":"None","session":true}]"#;
        let records: Vec<CookieRecord> = serde_json::from_slice(json).expect("deserialize");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].expires, None);
    }

    #[test]
    fn cookies_already_in_the_live_store_are_not_reinjected() {
        let now = 1_000;
        let records = vec![
            persistent_record("SID", now + 86_400),
            persistent_record("__Secure-3PSIDTS", now + 86_400),
        ];
        // The live store still holds a (possibly fresher) __Secure-3PSIDTS.
        let present: HashSet<CookieKey> = [(
            ".youtube.com".to_string(),
            "/".to_string(),
            "__Secure-3PSIDTS".to_string(),
        )]
        .into_iter()
        .collect();

        let picked = to_restore(&records, &present, now);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "SID", "must never overwrite a live cookie");
    }

    #[test]
    fn expired_records_are_not_reinjected() {
        let now = 10_000;
        let records = vec![
            persistent_record("__Secure-3PSIDRTS", now - 1), // short-TTL, already dead
            persistent_record("SID", now + 86_400),
        ];
        let picked = to_restore(&records, &HashSet::new(), now);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "SID");
    }

    #[test]
    fn session_records_are_restored_when_missing() {
        let now = 10_000;
        let mut record = persistent_record("YSC", 0);
        record.session = true;
        record.expires = None;
        let records = [record];
        let picked = to_restore(&records, &HashSet::new(), now);
        assert_eq!(picked.len(), 1, "a session cookie has no expiry to compare");
    }
}
