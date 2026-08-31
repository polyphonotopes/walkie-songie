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
    use std::{cell::RefCell, rc::Rc};

    // `indexedDB` is exposed on both Window and WorkerGlobalScope. Resolve the
    // global mixin directly so the exact Room-v5 log can move into a dedicated
    // worker without changing databases or copying history through the window.
    let global = js_sys::global();
    let indexed_db = js_sys::Reflect::get(&global, &JsValue::from_str("indexedDB"))
        .map_err(|_| "IndexedDB not available")?;
    if indexed_db.is_null() || indexed_db.is_undefined() {
        return Err("IndexedDB not available".into());
    }
    let idb_factory: web_sys::IdbFactory = indexed_db
        .dyn_into()
        .map_err(|_| "indexedDB global has an unexpected type")?;

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

    // Wait for success or failure. Keeping one shared sender makes the first
    // terminal event win and, unlike the old success-only path, guarantees an
    // IndexedDB open error cannot strand room entry forever.
    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = Rc::new(RefCell::new(Some(tx)));

    let success_tx = tx.clone();
    let onsuccess = Closure::once(Box::new(move |event: web_sys::Event| {
        let target = event.target().unwrap();
        let request: IdbRequest = target.unchecked_into();
        let db: IdbDatabase = request.result().unwrap().unchecked_into();
        if let Some(tx) = success_tx.borrow_mut().take() {
            let _ = tx.send(Ok(db));
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));

    let error_tx = tx;
    let onerror = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = error_tx.borrow_mut().take() {
            let _ = tx.send(Err("Failed to open IndexedDB".to_owned()));
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onerror(Some(onerror.as_ref().unchecked_ref()));

    let result = rx.await.map_err(|_| "IndexedDB open callback closed")?;
    request.set_onupgradeneeded(None);
    request.set_onsuccess(None);
    request.set_onerror(None);
    result
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
/// transport. One seed derives both the HHHS presentation key and the iroh
/// endpoint secret (see `net::identity`).
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

// ---------------------------------------------------------------------------
// Capability-native Room-v5 Replica transaction logs.
// ---------------------------------------------------------------------------

/// Load a byte blob from the settings store. Absence is distinct from an
/// IndexedDB failure so room recovery can refuse unavailable durable history.
#[cfg(feature = "browser-net")]
async fn get_bytes(db: &IdbDatabase, key: &str) -> Result<Option<Vec<u8>>, String> {
    use std::{cell::RefCell, rc::Rc};

    let transaction = db
        .transaction_with_str(STORE_NAME)
        .map_err(|_| "Failed to create read transaction")?;
    let store: IdbObjectStore = transaction
        .object_store(STORE_NAME)
        .map_err(|_| "Failed to get store")?;

    let request = store
        .get(&JsValue::from_str(key))
        .map_err(|_| "Failed to get journal")?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = Rc::new(RefCell::new(Some(tx)));

    let success_tx = tx.clone();
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
        if let Some(tx) = success_tx.borrow_mut().take() {
            let _ = tx.send(Ok(bytes));
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));

    let error_tx = tx;
    let onerror = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = error_tx.borrow_mut().take() {
            let _ = tx.send(Err("IndexedDB journal read failed".to_owned()));
        }
    }) as Box<dyn FnOnce(_)>);
    request.set_onerror(Some(onerror.as_ref().unchecked_ref()));

    let result = rx.await.map_err(|_| "IndexedDB read callback closed")?;
    request.set_onsuccess(None);
    request.set_onerror(None);
    result
}

/// Persist one byte blob and await the IndexedDB transaction-complete event.
/// Request success alone is not durable because the containing transaction may
/// still abort afterward.
#[cfg(feature = "browser-net")]
async fn put_bytes(db: &IdbDatabase, key: &str, bytes: &[u8]) -> Result<(), String> {
    use std::{cell::RefCell, rc::Rc};

    let transaction = db
        .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
        .map_err(|_| "Failed to create write transaction")?;
    let store: IdbObjectStore = transaction
        .object_store(STORE_NAME)
        .map_err(|_| "Failed to get store")?;
    let value = js_sys::Uint8Array::from(bytes);
    store
        .put_with_key(&value, &JsValue::from_str(key))
        .map_err(|_| "Failed to queue durable byte write")?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = Rc::new(RefCell::new(Some(tx)));
    let complete_tx = tx.clone();
    let oncomplete = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = complete_tx.borrow_mut().take() {
            let _ = tx.send(Ok(()));
        }
    }) as Box<dyn FnOnce(_)>);
    transaction.set_oncomplete(Some(oncomplete.as_ref().unchecked_ref()));
    let error_tx = tx.clone();
    let onerror = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = error_tx.borrow_mut().take() {
            let _ = tx.send(Err("IndexedDB byte transaction failed".to_owned()));
        }
    }) as Box<dyn FnOnce(_)>);
    transaction.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    let abort_tx = tx;
    let onabort = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = abort_tx.borrow_mut().take() {
            let _ = tx.send(Err("IndexedDB byte transaction aborted".to_owned()));
        }
    }) as Box<dyn FnOnce(_)>);
    transaction.set_onabort(Some(onabort.as_ref().unchecked_ref()));

    #[allow(deprecated)]
    if transaction.commit().is_err() {
        transaction.set_oncomplete(None);
        transaction.set_onerror(None);
        transaction.set_onabort(None);
        return Err("Failed to commit IndexedDB byte transaction".to_owned());
    }
    let result = rx
        .await
        .map_err(|_| "IndexedDB byte transaction callback closed")?;
    transaction.set_oncomplete(None);
    transaction.set_onerror(None);
    transaction.set_onabort(None);
    result
}

/// IndexedDB owner for rollback-sensitive pair-session renewal floors.
#[cfg(feature = "browser-net")]
#[derive(Default)]
pub struct IndexedDbSessionRenewalStore;

#[cfg(feature = "browser-net")]
impl crate::room::session::RoomSessionRenewalStore for IndexedDbSessionRenewalStore {
    fn load<'a>(
        &'a self,
        key: crate::room::session::RoomSessionRenewalKey,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Vec<u8>>, String>> + 'a>>
    {
        Box::pin(async move {
            let db = open_db().await?;
            get_bytes(&db, &session_renewal_key(key)).await
        })
    }

    fn persist<'a>(
        &'a self,
        key: crate::room::session::RoomSessionRenewalKey,
        floor: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + 'a>> {
        Box::pin(async move {
            let db = open_db().await?;
            put_bytes(&db, &session_renewal_key(key), &floor).await
        })
    }
}

#[cfg(feature = "browser-net")]
fn session_renewal_key(key: crate::room::session::RoomSessionRenewalKey) -> String {
    format!(
        "session-renewal:v1:{}:{}:{}",
        hhhs::Digest(key.room).to_hex(),
        crate::net::PeerId(key.local).to_hex(),
        crate::net::PeerId(key.peer).to_hex(),
    )
}

/// Append one encoded transaction and advance its manifest in the same
/// IndexedDB transaction. Request success alone is not the durability boundary:
/// the transaction can still abort afterward.
#[cfg(feature = "browser-net")]
async fn append_replica_transaction(
    db: &IdbDatabase,
    record_key: &str,
    record: &[u8],
    count_key: &str,
    next_count: u64,
) -> Result<(), String> {
    use std::{cell::RefCell, rc::Rc};

    let transaction = db
        .transaction_with_str_and_mode(STORE_NAME, web_sys::IdbTransactionMode::Readwrite)
        .map_err(|_| "Failed to create transaction")?;
    let store: IdbObjectStore = transaction
        .object_store(STORE_NAME)
        .map_err(|_| "Failed to get store")?;

    let arr = js_sys::Uint8Array::from(record);
    store
        .put_with_key(&arr, &JsValue::from_str(record_key))
        .map_err(|_| "Failed to append replica transaction")?;
    let count = js_sys::Uint8Array::from(next_count.to_le_bytes().as_slice());
    store
        .put_with_key(&count, &JsValue::from_str(count_key))
        .map_err(|_| "Failed to advance replica manifest")?;

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = Rc::new(RefCell::new(Some(tx)));

    let complete_tx = tx.clone();
    let oncomplete = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = complete_tx.borrow_mut().take() {
            let _ = tx.send(Ok(()));
        }
    }) as Box<dyn FnOnce(_)>);
    transaction.set_oncomplete(Some(oncomplete.as_ref().unchecked_ref()));

    let error_tx = tx.clone();
    let onerror = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = error_tx.borrow_mut().take() {
            let _ = tx.send(Err("IndexedDB journal transaction failed".to_owned()));
        }
    }) as Box<dyn FnOnce(_)>);
    transaction.set_onerror(Some(onerror.as_ref().unchecked_ref()));

    let abort_tx = tx;
    let onabort = Closure::once(Box::new(move |_event: web_sys::Event| {
        if let Some(tx) = abort_tx.borrow_mut().take() {
            let _ = tx.send(Err("IndexedDB journal transaction aborted".to_owned()));
        }
    }) as Box<dyn FnOnce(_)>);
    transaction.set_onabort(Some(onabort.as_ref().unchecked_ref()));

    // Both writes have been queued; begin committing without waiting for their
    // success events to make an otherwise unnecessary trip through a busy
    // browser event loop. Atomicity and the transaction `complete` boundary are
    // unchanged.
    // `IDBTransaction.commit()` is standardized and supported by the browsers
    // in Walkie's compatibility set. The pinned `web-sys` binding still marks
    // it deprecated, so keep the allowance scoped to this one call.
    #[allow(deprecated)]
    if transaction.commit().is_err() {
        transaction.set_oncomplete(None);
        transaction.set_onerror(None);
        transaction.set_onabort(None);
        return Err("Failed to commit IndexedDB journal transaction".to_owned());
    }

    let result = rx
        .await
        .map_err(|_| "IndexedDB transaction callback closed")?;
    transaction.set_oncomplete(None);
    transaction.set_onerror(None);
    transaction.set_onabort(None);
    result
}

/// Async durable owner for one Room-v5 HHHS replica log.
///
/// This is deliberately not a `ReplicaStorage` implementation: IndexedDB is
/// asynchronous and browser handles are single-task values. The room host
/// serializes one writer per lane, persists a prepared transaction here, then
/// finalizes that same transaction through `RoomReplicas::commit_prepared`.
/// Each HHHS transaction is a separate IndexedDB value; steady-state commits
/// never rewrite earlier room history.
#[cfg(feature = "browser-net")]
pub struct IndexedDbReplicaLogV5 {
    db: IdbDatabase,
    prefix: String,
    transactions: Vec<hhhs_store::StorageTransaction>,
}

#[cfg(feature = "browser-net")]
impl IndexedDbReplicaLogV5 {
    pub async fn open(
        room: &crate::room::v5::RoomIdentity,
        owner: crate::room::v5::ActorId,
        lane: crate::room::v5::RoomLane,
    ) -> Result<Self, String> {
        let db = open_db().await?;
        let prefix = replica_log_prefix_v5(room, owner, lane);
        let count = match get_bytes(&db, &replica_count_key(&prefix)).await? {
            None => 0,
            Some(bytes) => {
                let bytes: [u8; 8] = bytes
                    .try_into()
                    .map_err(|_| "invalid Room-v5 replica manifest length")?;
                usize::try_from(u64::from_le_bytes(bytes))
                    .map_err(|_| "Room-v5 replica manifest exceeds this browser's limits")?
            }
        };
        let mut transactions = Vec::with_capacity(count);
        // Bound the number of simultaneous IndexedDB requests while avoiding
        // one event-loop round trip per historical transaction during replay.
        const LOAD_BATCH: usize = 64;
        for start in (0..count).step_by(LOAD_BATCH) {
            let end = count.min(start + LOAD_BATCH);
            let keys: Vec<_> = (start..end)
                .map(|sequence| replica_record_key(&prefix, sequence))
                .collect();
            let records =
                futures::future::try_join_all(keys.iter().map(|key| get_bytes(&db, key))).await?;
            for (offset, record) in records.into_iter().enumerate() {
                let sequence = start + offset;
                let record = record
                    .ok_or_else(|| format!("Room-v5 replica transaction {sequence} is missing"))?;
                transactions.push(hhhs_store::decode_storage_transaction(&record).map_err(
                    |error| format!("invalid Room-v5 replica transaction {sequence}: {error}"),
                )?);
            }
        }
        Ok(Self {
            db,
            prefix,
            transactions,
        })
    }

    /// Decode the complete validated log for replay into a fresh memory store.
    pub fn transactions(&self) -> Result<Vec<hhhs_store::StorageTransaction>, String> {
        Ok(self.transactions.clone())
    }

    /// Atomically append one transaction and advance the replay manifest. The
    /// caller must not publish the prepared admission until this resolves.
    pub async fn persist(
        &mut self,
        transaction: &hhhs_store::StorageTransaction,
    ) -> Result<(), String> {
        let sequence = self.transactions.len();
        let next_count = u64::try_from(sequence)
            .ok()
            .and_then(|sequence| sequence.checked_add(1))
            .ok_or_else(|| "Room-v5 replica manifest overflow".to_owned())?;
        let encoded = hhhs_store::encode_storage_transaction(transaction);
        append_replica_transaction(
            &self.db,
            &replica_record_key(&self.prefix, sequence),
            &encoded,
            &replica_count_key(&self.prefix),
            next_count,
        )
        .await?;
        self.transactions.push(transaction.clone());
        Ok(())
    }
}

#[cfg(feature = "browser-net")]
impl hhhs_replica::AsyncTransactionSink for IndexedDbReplicaLogV5 {
    type Error = String;

    async fn persist(
        &mut self,
        transaction: &hhhs_store::StorageTransaction,
    ) -> Result<(), Self::Error> {
        IndexedDbReplicaLogV5::persist(self, transaction).await
    }
}

#[cfg(feature = "browser-net")]
fn replica_log_prefix_v5(
    room: &crate::room::v5::RoomIdentity,
    owner: crate::room::v5::ActorId,
    lane: crate::room::v5::RoomLane,
) -> String {
    format!(
        "replica-records:v5:{}:{}:{:02x}",
        room.object.to_hex(),
        crate::net::PeerId(owner.0).to_hex(),
        lane.tag()
    )
}

#[cfg(feature = "browser-net")]
fn replica_count_key(prefix: &str) -> String {
    format!("{prefix}:count")
}

#[cfg(feature = "browser-net")]
fn replica_record_key(prefix: &str, sequence: usize) -> String {
    format!("{prefix}:transaction:{sequence:020}")
}
