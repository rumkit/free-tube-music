/// Injected into every page load so a gear icon is always available to jump
/// back to the config page, even once navigated into music.youtube.com.
/// No-ops on the app's own local origins (config page), where the gear
/// would just navigate to the page it's already on.
pub const GEAR_OVERLAY_JS: &str = r#"
(function () {
  const LOCAL_HOSTNAMES = ["tauri.localhost", "localhost", "127.0.0.1"];
  if (LOCAL_HOSTNAMES.indexOf(location.hostname) !== -1) return;

  if (window.__gearInjected) return;
  window.__gearInjected = true;

  const NORMAL_BG = "rgba(20,20,20,0.75)";
  const ERROR_BG = "rgba(180,30,30,0.9)";
  const NORMAL_TITLE = "FreeTubeMusic settings";

  function showError(btn, message) {
    console.error("[FreeTubeMusic gear] " + message);
    btn.style.background = ERROR_BG;
    btn.title = "Settings unavailable: " + message;
    setTimeout(() => {
      btn.style.background = NORMAL_BG;
      btn.title = NORMAL_TITLE;
    }, 4000);
  }

  function inject() {
    if (document.getElementById("ftm-gear-btn")) return;

    const btn = document.createElement("button");
    btn.id = "ftm-gear-btn";
    btn.title = NORMAL_TITLE;
    btn.textContent = "⚙";
    btn.style.cssText = [
      "position:fixed",
      "top:10px",
      "right:10px",
      "z-index:2147483647",
      "width:32px",
      "height:32px",
      "border-radius:50%",
      "border:none",
      "background:" + NORMAL_BG,
      "color:#fff",
      "font-size:16px",
      "cursor:pointer",
      "line-height:32px",
      "padding:0",
      "box-shadow:0 1px 4px rgba(0,0,0,0.4)",
    ].join(";");

    btn.addEventListener("click", () => {
      if (window.__TAURI__ && window.__TAURI__.core) {
        window.__TAURI__.core.invoke("show_config").catch((err) => {
          showError(btn, String(err));
        });
      } else {
        showError(btn, "Tauri API not available");
      }
    });

    document.body.appendChild(btn);
  }

  if (document.body) {
    inject();
  } else {
    document.addEventListener("DOMContentLoaded", inject);
  }
})();
"#;
