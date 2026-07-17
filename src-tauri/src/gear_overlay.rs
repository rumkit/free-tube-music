/// Injected into every page load so a gear icon is always available to jump
/// back to the config page, even once navigated into music.youtube.com.
pub const GEAR_OVERLAY_JS: &str = r#"
(function () {
  if (window.__gearInjected) return;
  window.__gearInjected = true;

  function inject() {
    if (document.getElementById("ftm-gear-btn")) return;

    const btn = document.createElement("button");
    btn.id = "ftm-gear-btn";
    btn.title = "FreeTubeMusic settings";
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
      "background:rgba(20,20,20,0.75)",
      "color:#fff",
      "font-size:16px",
      "cursor:pointer",
      "line-height:32px",
      "padding:0",
      "box-shadow:0 1px 4px rgba(0,0,0,0.4)",
    ].join(";");

    btn.addEventListener("click", () => {
      if (window.__TAURI__ && window.__TAURI__.core) {
        window.__TAURI__.core.invoke("show_config");
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
