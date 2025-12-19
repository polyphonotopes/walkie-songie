//! IndexedDB storage for persisting app data.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{IdbDatabase, IdbObjectStore, IdbRequest};

const DB_NAME: &str = "walkie-songie";
const DB_VERSION: u32 = 1;
const STORE_NAME: &str = "settings";
const PEER_ID_KEY: &str = "peer_id";

/// Open the IndexedDB database.
async fn open_db() -> Result<IdbDatabase, String> {
    let window = web_sys::window().ok_or("No window")?;
    let idb_factory = window
        .indexed_db()
        .map_err(|_| "IndexedDB not available")?
        .ok_or("IndexedDB not available")?;

    let request = idb_factory
        .open_with_u32(DB_NAME, DB_VERSION)
        .map_err(|_| "Failed to open DB")?;

    // Handle upgrade needed (create object store)
    let onupgrade = Closure::once(Box::new(move |event: web_sys::IdbVersionChangeEvent| {
        let target = event.target().unwrap();
        let request: IdbRequest = target.unchecked_into();
        let db: IdbDatabase = request.result().unwrap().unchecked_into();

        // Always try to create the object store (will fail silently if exists)
        let _ = db.create_object_store(STORE_NAME);
    }) as Box<dyn FnOnce(_)>);
    request.set_onupgradeneeded(Some(onupgrade.as_ref().unchecked_ref()));
    onupgrade.forget();

    // Wait for success
    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::cell::RefCell::new(Some(tx));

    let onsuccess = Closure::once(Box::new(move |event: web_sys::Event| {
        let target = event.target().unwrap();
        let request: IdbRequest = target.unchecked_into();
        let db: IdbDatabase = request.result().unwrap().unchecked_into();
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(Ok(db));
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
    onsuccess.forget();

    let onerror = Closure::once(Box::new(move |_event: web_sys::Event| {
        // Error case - channel already consumed or we ignore
    }) as Box<dyn FnOnce(_)>);
    request.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    rx.await.map_err(|_| "Channel closed")?
}

/// Get the stored peer ID, or None if not set.
pub async fn get_peer_id() -> Option<String> {
    let db = open_db().await.ok()?;

    let transaction = db
        .transaction_with_str(STORE_NAME)
        .ok()?;
    let store: IdbObjectStore = transaction.object_store(STORE_NAME).ok()?;

    let request = store.get(&JsValue::from_str(PEER_ID_KEY)).ok()?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::cell::RefCell::new(Some(tx));

    let onsuccess = Closure::once(Box::new(move |event: web_sys::Event| {
        let target = event.target().unwrap();
        let request: IdbRequest = target.unchecked_into();
        let result = request.result().ok();
        let value = result.and_then(|v| v.as_string());
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(value);
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
    onsuccess.forget();

    rx.await.ok().flatten()
}

/// Store the peer ID.
pub async fn set_peer_id(peer_id: &str) -> Result<(), String> {
    let db = open_db().await?;

    let transaction = db
        .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
        .map_err(|_| "Failed to create transaction")?;
    let store: IdbObjectStore = transaction
        .object_store(STORE_NAME)
        .map_err(|_| "Failed to get store")?;

    let request = store
        .put_with_key(&JsValue::from_str(peer_id), &JsValue::from_str(PEER_ID_KEY))
        .map_err(|_| "Failed to put")?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::cell::RefCell::new(Some(tx));

    let onsuccess = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(Ok(()));
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
    onsuccess.forget();

    rx.await.map_err(|_| "Channel closed")?
}

/// Get or create a persistent peer ID.
pub async fn get_or_create_peer_id() -> String {
    // Try to load existing
    if let Some(peer_id) = get_peer_id().await {
        web_sys::console::log_1(&format!("Loaded peer ID from IndexedDB: {}", peer_id).into());
        return peer_id;
    }

    // Generate new
    let peer_id = format!("peer-{}", uuid::Uuid::new_v4());
    web_sys::console::log_1(&format!("Generated new peer ID: {}", peer_id).into());

    // Store it
    if let Err(e) = set_peer_id(&peer_id).await {
        web_sys::console::warn_1(&format!("Failed to store peer ID: {}", e).into());
    }

    peer_id
}

/// Get the stored room state for a given room name.
pub async fn get_room_state(room_name: &str) -> Option<Vec<u8>> {
    let db = open_db().await.ok()?;

    let transaction = db
        .transaction_with_str(STORE_NAME)
        .ok()?;
    let store: IdbObjectStore = transaction.object_store(STORE_NAME).ok()?;

    let key = format!("room:{}", room_name);
    let request = store.get(&JsValue::from_str(&key)).ok()?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::cell::RefCell::new(Some(tx));

    let onsuccess = Closure::once(Box::new(move |event: web_sys::Event| {
        let target = event.target().unwrap();
        let request: IdbRequest = target.unchecked_into();
        let result = request.result().ok();
        // Convert Uint8Array to Vec<u8>
        let bytes = result.and_then(|v| {
            if v.is_undefined() || v.is_null() {
                return None;
            }
            let arr = js_sys::Uint8Array::new(&v);
            Some(arr.to_vec())
        });
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(bytes);
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
    onsuccess.forget();

    rx.await.ok().flatten()
}

/// Store the room state for a given room name.
pub async fn set_room_state(room_name: &str, state: &[u8]) -> Result<(), String> {
    let db = open_db().await?;

    let transaction = db
        .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
        .map_err(|_| "Failed to create transaction")?;
    let store: IdbObjectStore = transaction
        .object_store(STORE_NAME)
        .map_err(|_| "Failed to get store")?;

    let key = format!("room:{}", room_name);
    let arr = js_sys::Uint8Array::from(state);

    let request = store
        .put_with_key(&arr, &JsValue::from_str(&key))
        .map_err(|_| "Failed to put")?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::cell::RefCell::new(Some(tx));

    let onsuccess = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(Ok(()));
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
    onsuccess.forget();

    rx.await.map_err(|_| "Channel closed")?
}
