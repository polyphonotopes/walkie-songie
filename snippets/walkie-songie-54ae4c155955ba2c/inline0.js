
export function walkieTauriAvailable() {
  return Boolean(window.__TAURI__?.core?.invoke && window.__TAURI__?.core?.Channel);
}

export function walkieDispatchJson(json) {
  const command = JSON.parse(json);
  return window.__TAURI__.core.invoke("dispatch", { command }).catch((error) => {
    const message = typeof error === "string" ? error : JSON.stringify(error);
    return Promise.reject(message);
  });
}

export function walkieRegisterEvents(callback) {
  const channel = new window.__TAURI__.core.Channel();
  channel.onmessage = (message) => callback(JSON.stringify(message));
  window.__walkieSongieEventChannel = channel;
  return window.__TAURI__.core.invoke("register_events", { onEvent: channel });
}
