const { invoke } = window.__TAURI__.core;

const form = document.getElementById("config-form");
const errorBox = document.getElementById("form-error");
const saveBtn = document.getElementById("save-btn");
const restartBtn = document.getElementById("restart-btn");

function showError(message) {
  errorBox.textContent = message;
  errorBox.hidden = false;
}

function clearError() {
  errorBox.hidden = true;
  errorBox.textContent = "";
}

function setRedirectMode(mode) {
  const radio = form.querySelector(`input[name="redirect_mode"][value="${mode}"]`);
  if (radio) radio.checked = true;
}

async function loadConfig() {
  try {
    const config = await invoke("load_config");
    form.proxy_host.value = config.proxy_host ?? "";
    form.proxy_port.value = config.proxy_port ?? "";
    form.proxy_username.value = config.proxy_username ?? "";
    form.router_port.value = config.router_port ?? 9090;
    setRedirectMode(config.redirect_mode ?? "list");
    form.redirect_hosts.value = (config.redirect_hosts ?? []).join("\n");
    form.main_host.value = config.main_host ?? "https://music.youtube.com";
    // password intentionally left blank
  } catch (err) {
    showError(`Failed to load config: ${err}`);
  }
}

function buildConfigPayload() {
  const redirect_hosts = form.redirect_hosts.value
    .split("\n")
    .map((h) => h.trim())
    .filter(Boolean);

  return {
    proxy_host: form.proxy_host.value.trim(),
    proxy_port: Number(form.proxy_port.value),
    proxy_username: form.proxy_username.value.trim(),
    router_port: Number(form.router_port.value),
    redirect_mode: form.querySelector('input[name="redirect_mode"]:checked').value,
    redirect_hosts,
    main_host: form.main_host.value.trim(),
  };
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  clearError();
  saveBtn.disabled = true;
  saveBtn.textContent = "Saving...";

  try {
    const config = buildConfigPayload();
    const password = form.proxy_password.value.length > 0 ? form.proxy_password.value : null;

    const result = await invoke("save_config", { config, password });

    if (result.restart_required) {
      restartBtn.hidden = false;
      saveBtn.hidden = true;
    } else {
      await invoke("apply_and_launch");
    }
  } catch (err) {
    showError(typeof err === "string" ? err : String(err));
  } finally {
    saveBtn.disabled = false;
    saveBtn.textContent = "Save & Launch";
  }
});

restartBtn.addEventListener("click", async () => {
  restartBtn.disabled = true;
  try {
    await invoke("request_restart");
  } catch (err) {
    showError(typeof err === "string" ? err : String(err));
    restartBtn.disabled = false;
  }
});

loadConfig();
