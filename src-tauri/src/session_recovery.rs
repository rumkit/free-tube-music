//! Automatic recovery of a lapsed YouTube session — the one behaviour the
//! YouTube Music PWA gets for free that this app does not.
//!
//! Google keeps a signed-in YouTube session fresh with a pair of rotation
//! tokens, `__Secure-1PSIDRTS` / `__Secure-3PSIDRTS`, which carry a **10 minute**
//! expiry on both `.google.com` and `.youtube.com`. While a browser is open they
//! are refreshed continuously and `__Secure-*PSIDTS` is re-minted with them.
//!
//! Close the app and ten minutes later those rotation tokens expire and are
//! purged. The next launch presents a `__Secure-*PSIDTS` that is hours stale with
//! nothing left to rotate it, so YouTube rejects the session and renders signed
//! out — even though `.google.com`'s master `SID`/`LSID`/`__Host-GAPS` are still
//! valid for 400 days. That asymmetry is exactly why clicking "Sign in" then
//! completes instantly with no password prompt.
//!
//! The PWA doesn't hit this because Chrome keeps running (rotation never lapses)
//! and, on a cold start, re-mints via DBSC against `accounts.google.com`. We
//! can't replicate the browser's device-bound session, but we can do what the
//! user does by hand: follow YouTube's own sign-in link, which bounces through
//! `accounts.google.com` and comes straight back with a fresh session.
//!
//! Design notes:
//!
//! - **Gate on the page's signed-out state, not on cookies.** A cookie-presence
//!   check in Rust would see the rotation tokens missing after any gap longer
//!   than 10 minutes and bounce on nearly every restart, including ones where the
//!   session is still perfectly fresh. `ytcfg`'s `LOGGED_IN` fires only when
//!   YouTube has actually rejected the session.
//! - **Prefer YouTube's own sign-in href.** Reading it off the live DOM means it
//!   can't rot when Google changes the flow. There is a constructed
//!   `ServiceLogin` fallback for when the scan finds nothing — the signed-out
//!   markup is the one thing here that couldn't be tested ahead of time, and a
//!   guess that might rot beats doing nothing and waiting another day to find
//!   out. The `sessionStorage` guard bounds the cost if the fallback is wrong.
//! - **One attempt per tab session.** The guard is checked *before* navigating
//!   and set immediately, and `sessionStorage` survives the round trip through
//!   `accounts.google.com`, so a bounce that fails to restore the session cannot
//!   loop. This is only sound because the script is top-frame-only (below) —
//!   a same-origin iframe shares `sessionStorage` and could otherwise burn the
//!   one-shot flag on the top document's behalf.
//! - **Top frame only.** `initialization_script` becomes
//!   `AddScriptToExecuteOnDocumentCreated`, which wry applies to subframes as
//!   well as the main document, and a YouTube-hosted iframe satisfies the
//!   hostname gate just as the real page does.

/// Injected on every page load, alongside the gear overlay.
pub const SESSION_RECOVERY_JS: &str = r#"
(function () {
  if (!/(^|\.)youtube\.com$/.test(location.hostname)) return;
  // This script is injected into subframes too, so a YouTube iframe would
  // otherwise run it: location.href would navigate the frame rather than the
  // window, and a same-origin frame shares sessionStorage, so it could burn the
  // one-shot guard and suppress the bounce that actually matters.
  if (window.top !== window.self) return;
  if (window.__ftmSessionRecovery) return;
  window.__ftmSessionRecovery = true;

  var ATTEMPT_KEY = "ftm-reauth-attempted";
  var DEADLINE_MS = 15000;
  var POLL_MS = 500;

  function report(message) {
    console.log("[FreeTubeMusic session] " + message);
    try {
      if (window.__TAURI__ && window.__TAURI__.core) {
        window.__TAURI__.core.invoke("log_page_event", { message: message });
      }
    } catch (e) {
      /* logging must never break the page */
    }
  }

  // true / false when YouTube has told us, null while still unknown.
  function loggedIn() {
    try {
      if (window.ytcfg && typeof ytcfg.get === "function") {
        var v = ytcfg.get("LOGGED_IN");
        if (typeof v === "boolean") return v;
      }
    } catch (e) {
      /* ytcfg not ready */
    }
    return null;
  }

  // YouTube's own sign-in anchor. Every surface points it at accounts.google.com
  // with a continue= back to where we are, which is exactly the bounce we want.
  // Returns null if the nav hasn't rendered yet, so the caller can keep polling.
  function signInHref() {
    var links = document.querySelectorAll('a[href*="accounts.google.com"]');
    for (var i = 0; i < links.length; i++) {
      var href = links[i].href;
      if (href.indexOf("continue=") !== -1 || href.indexOf("ServiceLogin") !== -1) {
        return href;
      }
    }
    return null;
  }

  // Used only once the deadline passes without the scan finding anything — for
  // instance if YouTube renders sign-in as a button rather than a light-DOM
  // anchor. Same shape as the link YouTube itself uses.
  function fallbackHref() {
    return "https://accounts.google.com/ServiceLogin?service=youtube&uilel=3" +
      "&continue=" + encodeURIComponent(location.href);
  }

  function attempted() {
    try {
      return sessionStorage.getItem(ATTEMPT_KEY) === "1";
    } catch (e) {
      // No sessionStorage means no loop guard, so treat it as "already tried"
      // rather than risk a redirect loop.
      return true;
    }
  }

  function markAttempted() {
    try {
      sessionStorage.setItem(ATTEMPT_KEY, "1");
      return true;
    } catch (e) {
      return false;
    }
  }

  var started = Date.now();

  function check() {
    var state = loggedIn();

    if (state === true) {
      report("signed in");
      return;
    }

    if (state === false) {
      if (attempted()) {
        report("signed out, but a re-auth was already attempted this session — leaving it alone");
        return;
      }
      var href = signInHref();
      var viaFallback = false;
      if (!href) {
        // The nav bar may not have rendered yet; keep looking until the deadline.
        if (Date.now() - started < DEADLINE_MS) {
          setTimeout(check, POLL_MS);
          return;
        }
        href = fallbackHref();
        viaFallback = true;
      }
      if (!markAttempted()) {
        report("signed out but sessionStorage is unavailable — not bouncing (no loop guard)");
        return;
      }
      report(
        viaFallback
          ? "signed out and no sign-in link found; bouncing via the constructed ServiceLogin URL"
          : "signed out; bouncing through YouTube's own sign-in link to refresh the session"
      );
      location.href = href;
      return;
    }

    if (Date.now() - started < DEADLINE_MS) {
      setTimeout(check, POLL_MS);
    } else {
      report("could not determine sign-in state within " + DEADLINE_MS + "ms");
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", check);
  } else {
    check();
  }
})();
"#;
