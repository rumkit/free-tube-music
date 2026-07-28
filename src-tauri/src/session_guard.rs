/// Injected into every page load to auto-recover the YT Music session.
///
/// Google rotates its session-token cookies (`__Secure-*PSIDTS`) on open tabs
/// every ~30 minutes. If the machine sleeps with the app open, rotation stalls
/// and on wake the server rejects the stale token: YT Music shows "Your Google
/// Account was signed out on a different tab" (mid-session dialog), or renders
/// its signed-out start page on the next launch. The long-lived Google auth
/// cookies are still valid at that point, so one round-trip through
/// `accounts.google.com/ServiceLogin?passive=true` re-establishes the session
/// with no user interaction (`passive` never shows login UI — if silent
/// sign-in isn't possible it just redirects back).
///
/// Guardrails: we only attempt re-auth if this profile has been seen signed in
/// before (never push a never-signed-in user into a login flow), and at most
/// twice per 10 minutes so an intentional sign-out can't loop.
pub const SESSION_GUARD_JS: &str = r#"
(function () {
  if (!/(^|\.)music\.youtube\.com$/.test(location.hostname)) return;
  if (window.__ftmSessionGuard) return;
  window.__ftmSessionGuard = true;

  const FLAG_KEY = "ftm-was-signed-in";
  const ATTEMPTS_KEY = "ftm-reauth-attempts";
  const MAX_ATTEMPTS = 2;
  const WINDOW_MS = 10 * 60 * 1000;

  // Same redirect chain YT Music's own "Sign in" button uses: establish the
  // session on accounts.google.com, propagate it via youtube.com/signin, land
  // back on music.youtube.com.
  const CONTINUE_URL =
    "https://www.youtube.com/signin?action_handle_signin=true&app=desktop&next=" +
    encodeURIComponent("https://music.youtube.com/");
  const REAUTH_URL =
    "https://accounts.google.com/ServiceLogin?service=youtube&passive=true&continue=" +
    encodeURIComponent(CONTINUE_URL);

  function wasSignedIn() {
    try { return localStorage.getItem(FLAG_KEY) === "1"; } catch (e) { return false; }
  }

  function recentAttempts() {
    try {
      const now = Date.now();
      const list = JSON.parse(localStorage.getItem(ATTEMPTS_KEY) || "[]");
      return list.filter((t) => typeof t === "number" && now - t < WINDOW_MS);
    } catch (e) { return []; }
  }

  function attemptReauth(reason) {
    if (!wasSignedIn()) return;
    const attempts = recentAttempts();
    if (attempts.length >= MAX_ATTEMPTS) return;
    attempts.push(Date.now());
    try { localStorage.setItem(ATTEMPTS_KEY, JSON.stringify(attempts)); } catch (e) {}
    console.info("[FreeTubeMusic] session lost (" + reason + "), re-authenticating silently");
    location.href = REAUTH_URL;
  }

  // true / false when ytcfg has loaded and knows, null while indeterminate.
  function loggedIn() {
    try {
      const cfg = window.ytcfg;
      if (!cfg) return null;
      if (typeof cfg.get === "function") {
        const v = cfg.get("LOGGED_IN");
        return typeof v === "boolean" ? v : null;
      }
      if (cfg.data_ && typeof cfg.data_.LOGGED_IN === "boolean") return cfg.data_.LOGGED_IN;
      return null;
    } catch (e) { return null; }
  }

  // On load: page rendered signed out although the user was signed in before.
  let checks = 0;
  const poll = setInterval(() => {
    checks += 1;
    const state = loggedIn();
    if (state === true) {
      clearInterval(poll);
      try {
        localStorage.setItem(FLAG_KEY, "1");
        localStorage.removeItem(ATTEMPTS_KEY);
      } catch (e) {}
    } else if (state === false) {
      clearInterval(poll);
      attemptReauth("page loaded signed out");
    } else if (checks >= 15) {
      clearInterval(poll);
    }
  }, 1000);

  // Mid-session: the "signed out on a different tab" dialog appears; its
  // sign-in action links to ServiceLogin. Selector-based so it's
  // language-independent.
  const DIALOG_LINK = [
    'ytmusic-popup-container a[href*="accounts.google.com/ServiceLogin"]',
    'tp-yt-paper-dialog a[href*="accounts.google.com/ServiceLogin"]',
    'ytmusic-dialog a[href*="accounts.google.com/ServiceLogin"]',
  ].join(",");

  let scanScheduled = false;
  function scheduleScan() {
    if (scanScheduled) return;
    scanScheduled = true;
    setTimeout(() => {
      scanScheduled = false;
      try {
        if (document.querySelector(DIALOG_LINK)) attemptReauth("sign-out dialog shown");
      } catch (e) {}
    }, 500);
  }

  function startObserver() {
    new MutationObserver(scheduleScan)
      .observe(document.documentElement, { childList: true, subtree: true });
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", startObserver);
  } else {
    startObserver();
  }
})();
"#;
