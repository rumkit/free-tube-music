//! Disable WebView2's tracking prevention for the app's webview.
//!
//! WebView2 ships with tracking prevention at "Balanced" by default, which
//! blocks *third-party* (cross-site) cookies — something a full browser or an
//! installed PWA does not do for Google's own sign-in. YouTube Music's logged-in
//! session is cross-site by construction: auth and the rotating `__Secure-*PSIDTS`
//! freshness tokens live on `google.com`/`accounts.google.com`, while the content
//! is on `youtube.com`/`music.youtube.com`. With third-party cookies blocked, the
//! rotated session tokens never stick and the server signs the user out
//! mid-session. Setting the profile's tracking-prevention level to `None` lets
//! those cross-site cookies flow like they do in the PWA.
//!
//! There is no Tauri config or `additionalBrowserArguments` lever for this — the
//! WebView2 API exposes it only via the `EnableTrackingPrevention` environment
//! option (owned by Tauri, not exposed) or the profile property we set here at
//! runtime through the `with_webview` COM escape hatch.

use tauri::WebviewWindow;

/// Set the webview profile's tracking-prevention level to `None`. Best-effort:
/// every failure (older runtime without the interface, COM error) is logged and
/// swallowed so it can never block startup.
#[cfg(windows)]
pub fn disable(window: &WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Profile3, ICoreWebView2_13,
        COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_NONE,
    };
    use windows::core::Interface;

    let result = window.with_webview(|webview| unsafe {
        let core = match webview.controller().CoreWebView2() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("tracking prevention: CoreWebView2() failed: {e}");
                return;
            }
        };
        // Profile is exposed from ICoreWebView2_13; the level setter from
        // ICoreWebView2Profile3. Both need a reasonably recent runtime.
        let profile = match core.cast::<ICoreWebView2_13>().and_then(|c| c.Profile()) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("tracking prevention: profile unavailable: {e}");
                return;
            }
        };
        match profile
            .cast::<ICoreWebView2Profile3>()
            .and_then(|p| p.SetPreferredTrackingPreventionLevel(
                COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_NONE,
            )) {
            Ok(()) => log::debug!("tracking prevention disabled (level None)"),
            Err(e) => log::warn!("tracking prevention: setting level failed: {e}"),
        }
    });
    if let Err(e) = result {
        log::warn!("tracking prevention: with_webview failed: {e}");
    }
}

#[cfg(not(windows))]
pub fn disable(_window: &WebviewWindow) {}
