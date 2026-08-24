//! Thin typed adapter over Tauri's global JavaScript IPC object.

use std::rc::Rc;

use js_sys::{Function, Promise};
use wasm_bindgen::{JsCast, closure::Closure, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::client::{AppEventEnvelope, ClientCommand};

#[wasm_bindgen(inline_js = r#"
let walkieCommandQueue = Promise.resolve();

export function walkieTauriAvailable() {
  return Boolean(window.__TAURI__?.core?.invoke && window.__TAURI__?.core?.Channel);
}

export function walkieDispatchJson(json) {
  const command = JSON.parse(json);
  const request = walkieCommandQueue.then(() =>
    window.__TAURI__.core.invoke("dispatch", { command })
  );
  // Keep the ingress alive after an individual rejection while returning the
  // original request to its caller. This preserves UI command order across the
  // async IPC boundary (notably EnterRoom followed immediately by a note).
  walkieCommandQueue = request.catch(() => {});
  return request.catch((error) => {
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
"#)]
extern "C" {
    #[wasm_bindgen(js_name = walkieTauriAvailable)]
    fn tauri_available() -> bool;
    #[wasm_bindgen(js_name = walkieDispatchJson)]
    fn dispatch_json(json: &str) -> Promise;
    #[wasm_bindgen(js_name = walkieRegisterEvents)]
    fn register_events_js(callback: &Function) -> Promise;
}

pub fn is_available() -> bool {
    tauri_available()
}

pub fn dispatch(command: ClientCommand, on_error: impl Fn(String) + 'static) {
    let Ok(json) = serde_json::to_string(&command) else {
        return;
    };
    spawn_local(async move {
        if let Err(error) = JsFuture::from(dispatch_json(&json)).await {
            let message = error.as_string().unwrap_or_else(|| format!("{error:?}"));
            web_sys::console::error_1(&error);
            on_error(message);
        }
    });
}

pub async fn register_events(handler: impl Fn(AppEventEnvelope) + 'static) -> Result<(), JsValue> {
    let handler = Rc::new(handler);
    let callback = Closure::<dyn FnMut(JsValue)>::new(move |json: JsValue| {
        let Some(json) = json.as_string() else {
            web_sys::console::error_1(&"native event channel sent a non-string payload".into());
            return;
        };
        match serde_json::from_str::<AppEventEnvelope>(&json) {
            Ok(envelope) => handler(envelope),
            Err(error) => {
                web_sys::console::error_1(&format!("invalid native event envelope: {error}").into())
            }
        }
    });
    let promise = register_events_js(callback.as_ref().unchecked_ref());
    callback.forget();
    JsFuture::from(promise).await.map(|_| ())
}
