//! IndexedDB storage for persisting app data.

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
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

    let transaction = db.transaction_with_str(STORE_NAME).ok()?;
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

/// Key holding the 32-byte Ed25519 identity seed for the in-browser iroh
/// transport. One seed derives BOTH the p2panda signing key and the iroh
/// endpoint secret (see `net::identity`), so `author_id == endpoint_id`.
#[cfg(feature = "browser-net")]
const IDENTITY_SEED_KEY: &str = "identity_seed";

/// The persisted identity seed, if one exists and is exactly 32 bytes.
#[cfg(feature = "browser-net")]
pub async fn get_identity_seed() -> Option<[u8; 32]> {
    let db = open_db().await.ok()?;

    let transaction = db.transaction_with_str(STORE_NAME).ok()?;
    let store: IdbObjectStore = transaction.object_store(STORE_NAME).ok()?;

    let request = store.get(&JsValue::from_str(IDENTITY_SEED_KEY)).ok()?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::cell::RefCell::new(Some(tx));

    let onsuccess = Closure::once(Box::new(move |event: web_sys::Event| {
        let target = event.target().unwrap();
        let request: IdbRequest = target.unchecked_into();
        let result = request.result().ok();
        let bytes = result.and_then(|v| {
            if v.is_undefined() || v.is_null() {
                return None;
            }
            let arr = js_sys::Uint8Array::new(&v);
            let vec = arr.to_vec();
            <[u8; 32]>::try_from(vec).ok()
        });
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(bytes);
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
    onsuccess.forget();

    rx.await.ok().flatten()
}

/// Persist the identity seed.
#[cfg(feature = "browser-net")]
pub async fn set_identity_seed(seed: &[u8; 32]) -> Result<(), String> {
    let db = open_db().await?;

    let transaction = db
        .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
        .map_err(|_| "Failed to create transaction")?;
    let store: IdbObjectStore = transaction
        .object_store(STORE_NAME)
        .map_err(|_| "Failed to get store")?;

    let arr = js_sys::Uint8Array::from(seed.as_slice());
    let request = store
        .put_with_key(&arr, &JsValue::from_str(IDENTITY_SEED_KEY))
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

/// Load the identity seed, minting and persisting a fresh random one on first
/// run (`crypto.getRandomValues` via getrandom's `wasm_js` backend). A failed
/// write still returns the fresh seed — the tab works, identity is ephemeral.
#[cfg(feature = "browser-net")]
pub async fn get_or_create_identity_seed() -> [u8; 32] {
    if let Some(seed) = get_identity_seed().await {
        return seed;
    }
    let seed: [u8; 32] = rand::random();
    if let Err(error) = set_identity_seed(&seed).await {
        web_sys::console::warn_1(
            &format!("Failed to persist identity seed (ephemeral identity): {error}").into(),
        );
    }
    seed
}

/// Get the stored room state for a given room name.
pub async fn get_room_state(room_name: &str) -> Option<Vec<u8>> {
    let db = open_db().await.ok()?;

    let transaction = db.transaction_with_str(STORE_NAME).ok()?;
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

// ---------------------------------------------------------------------------
// Signed-op journal
//
// A per-room, additive cache of the room's admitted signed ops, keyed by the
// room topic hex. It seeds the `RoomStore` on start so a solo reload keeps its
// history, and grows on every admitted op. It is stored as ONE blob per topic
// under the existing settings object store (same simple get/put path as
// `get_room_state`/`set_room_state`), framed as length-prefixed records that
// mirror the native file journal: `u32-le length ++ verbatim signed-op wire
// bytes` per record. A read/write failure degrades gracefully — the store just
// starts empty and reconverges via gossip + anti-entropy, exactly as before.
// ---------------------------------------------------------------------------

/// The settings-store key holding a room's signed-op journal blob.
#[cfg(feature = "browser-net")]
fn op_journal_key(topic_hex: &str) -> String {
    format!("opjournal:{topic_hex}")
}

/// Frame verbatim signed-op wire records into a single blob.
#[cfg(feature = "browser-net")]
fn encode_op_journal(records: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = records.iter().map(|record| record.len() + 4).sum();
    let mut out = Vec::with_capacity(total);
    for record in records {
        out.extend_from_slice(&(record.len() as u32).to_le_bytes());
        out.extend_from_slice(record);
    }
    out
}

/// Recover verbatim signed-op wire records from a journal blob. A torn tail
/// (partial length or record) simply stops parsing — the completed prefix is
/// returned and anti-entropy backfills the rest.
#[cfg(feature = "browser-net")]
fn decode_op_journal(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= bytes.len() {
        let length = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("checked four bytes"),
        ) as usize;
        offset += 4;
        let Some(end) = offset.checked_add(length) else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        records.push(bytes[offset..end].to_vec());
        offset = end;
    }
    records
}

/// Load a byte blob from the settings store, or `None` on absence/failure.
#[cfg(feature = "browser-net")]
async fn get_bytes(key: &str) -> Option<Vec<u8>> {
    let db = open_db().await.ok()?;

    let transaction = db.transaction_with_str(STORE_NAME).ok()?;
    let store: IdbObjectStore = transaction.object_store(STORE_NAME).ok()?;

    let request = store.get(&JsValue::from_str(key)).ok()?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::cell::RefCell::new(Some(tx));

    let onsuccess = Closure::once(Box::new(move |event: web_sys::Event| {
        let target = event.target().unwrap();
        let request: IdbRequest = target.unchecked_into();
        let result = request.result().ok();
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

/// Write a byte blob to the settings store under `key`.
#[cfg(feature = "browser-net")]
async fn set_bytes(key: &str, bytes: &[u8]) -> Result<(), String> {
    let db = open_db().await?;

    let transaction = db
        .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
        .map_err(|_| "Failed to create transaction")?;
    let store: IdbObjectStore = transaction
        .object_store(STORE_NAME)
        .map_err(|_| "Failed to get store")?;

    let arr = js_sys::Uint8Array::from(bytes);
    let request = store
        .put_with_key(&arr, &JsValue::from_str(key))
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

/// Load a room's journaled signed-op wire records, keyed by the room topic hex.
/// Each entry is the exact verbatim bytes an author signed. Returns an empty
/// vec on absence or any failure — the caller then starts from an empty store.
#[cfg(feature = "browser-net")]
pub async fn get_op_journal(topic_hex: &str) -> Vec<Vec<u8>> {
    match get_bytes(&op_journal_key(topic_hex)).await {
        Some(blob) => decode_op_journal(&blob),
        None => Vec::new(),
    }
}

/// Persist a room's signed-op journal, keyed by the room topic hex. `records`
/// are the verbatim signed-op wire bytes in admit order. An error is returned
/// so the caller can log and continue; the journal is a best-effort local
/// cache and never blocks room entry.
#[cfg(feature = "browser-net")]
pub async fn set_op_journal(topic_hex: &str, records: &[Vec<u8>]) -> Result<(), String> {
    set_bytes(&op_journal_key(topic_hex), &encode_op_journal(records)).await
}
