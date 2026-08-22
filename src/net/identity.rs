//! One persisted Ed25519 seed, used across explicit protocol roles.
//!
//! A participant has a single 32-byte seed. That same seed feeds:
//!   * the HHHS proof key used to present room capabilities, and
//!   * the iroh endpoint key used to authenticate a connection.
//!
//! Sharing key material makes identity hand-off ergonomic; it does not collapse
//! protocol roles. An authenticated iroh connection grants no room authority.
//! Every durable command and signed presence update still presents a live,
//! object-scoped capability path.
//!
//! Persistence is intentionally out of scope here: [`SeedStore`] captures the tiny
//! load/save surface, and only the in-memory [`MemorySeedStore`] ships now. The real
//! backends (browser IndexedDB via `src/web/storage.rs`, native seed file, 0600) land
//! with the transport wiring.

#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
use std::path::{Path, PathBuf};

/// A participant identity derived from a persisted 32-byte Ed25519 seed.
///
/// Derives proof and transport keys from one durable seed.
#[derive(Clone)]
pub struct WalkieIdentity {
    seed: [u8; 32],
}

impl WalkieIdentity {
    /// Wrap a raw 32-byte seed (e.g. one just loaded from a [`SeedStore`]).
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { seed }
    }

    /// Load the seed from `store`, generating and persisting a fresh random one on
    /// first run. The randomness comes from `getrandom` (via `rand`): on wasm that is
    /// `crypto.getRandomValues` (getrandom 0.3 `wasm_js`), on native the OS RNG.
    pub fn load_or_create<S: SeedStore>(store: &S) -> Result<Self, S::Error> {
        match store.load()? {
            Some(seed) => Ok(Self { seed }),
            None => {
                let seed: [u8; 32] = rand::random();
                store.save(&seed)?;
                Ok(Self { seed })
            }
        }
    }

    /// The raw seed. Exposed only so a [`SeedStore`] can persist it — never put this on
    /// the wire.
    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    /// HHHS 0.4 capability-presentation key. It is derived from the same seed
    /// as the transport key, but connection authentication alone never grants
    /// authority: Room v5 still requires an explicit live capability path.
    pub fn capability_signing_key(&self) -> hhhs_proof::SigningKey {
        hhhs_proof::SigningKey::from_bytes(&self.seed)
    }

    pub fn capability_actor_id(&self) -> crate::room::v5::ActorId {
        crate::room::v5::ActorId::from_signing_key(&self.capability_signing_key())
    }

    /// The iroh endpoint secret key.
    #[cfg(any(feature = "native-net", feature = "browser-net"))]
    pub fn iroh_secret(&self) -> iroh::SecretKey {
        iroh::SecretKey::from_bytes(&self.seed)
    }
}

/// Durable storage for the 32-byte identity seed.
///
/// Kept minimal on purpose: the real browser (IndexedDB) and native (seed file)
/// backends implement this later; today only [`MemorySeedStore`] exists.
pub trait SeedStore {
    /// Failure type for the backend (e.g. an IndexedDB or filesystem error).
    type Error;

    /// The persisted seed, or `None` on first run (nothing stored yet).
    fn load(&self) -> Result<Option<[u8; 32]>, Self::Error>;

    /// Persist `seed` durably, overwriting any previous value.
    fn save(&self, seed: &[u8; 32]) -> Result<(), Self::Error>;
}

/// An in-memory [`SeedStore`] for tests and ephemeral (no-persistence) contexts.
///
/// Uses interior mutability so it satisfies the `&self` trait surface. Not durable
/// across process restarts; single-threaded (the transport layer is `!Send`).
#[derive(Default)]
pub struct MemorySeedStore {
    seed: std::cell::RefCell<Option<[u8; 32]>>,
}

impl MemorySeedStore {
    /// An empty store — the first [`WalkieIdentity::load_or_create`] mints a seed.
    pub fn new() -> Self {
        Self::default()
    }

    /// A store pre-populated with `seed` (deterministic identity for tests).
    pub fn with_seed(seed: [u8; 32]) -> Self {
        Self {
            seed: std::cell::RefCell::new(Some(seed)),
        }
    }
}

impl SeedStore for MemorySeedStore {
    type Error = std::convert::Infallible;

    fn load(&self) -> Result<Option<[u8; 32]>, Self::Error> {
        Ok(*self.seed.borrow())
    }

    fn save(&self, seed: &[u8; 32]) -> Result<(), Self::Error> {
        *self.seed.borrow_mut() = Some(*seed);
        Ok(())
    }
}

/// Native identity seed stored as an exact 32-byte file.
///
/// The parent directory and file receive owner-only permissions on Unix. Writes
/// go through a sibling temporary file, `sync_all`, and atomic rename so a crash
/// cannot turn a valid identity into a partial seed.
#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
#[derive(Debug, Clone)]
pub struct FileSeedStore {
    path: PathBuf,
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
impl FileSeedStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn ensure_parent(&self) -> std::io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "identity seed path has no parent",
            )
        })?;
        std::fs::create_dir_all(parent)?;
        set_private_directory_permissions(parent)?;
        Ok(())
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
impl SeedStore for FileSeedStore {
    type Error = std::io::Error;

    fn load(&self) -> Result<Option<[u8; 32]>, Self::Error> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let seed: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "identity seed must be exactly 32 bytes, found {}",
                            bytes.len()
                        ),
                    )
                })?;
                Ok(Some(seed))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn save(&self, seed: &[u8; 32]) -> Result<(), Self::Error> {
        use std::io::Write;

        self.ensure_parent()?;
        let temporary = self.path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        set_private_file_options(&mut options);
        let result = (|| {
            let mut file = options.open(&temporary)?;
            file.write_all(seed)?;
            file.sync_all()?;
            // Linking a fully synced temporary inode into the final name is an
            // atomic create-if-absent operation. Unlike rename, it cannot
            // replace an identity created concurrently by another process.
            std::fs::hard_link(&temporary, &self.path)?;
            std::fs::remove_file(&temporary)?;
            sync_parent_directory(&self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(all(unix, not(target_arch = "wasm32"), feature = "native-net"))]
fn set_private_directory_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(all(not(unix), not(target_arch = "wasm32"), feature = "native-net"))]
fn set_private_directory_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(unix, not(target_arch = "wasm32"), feature = "native-net"))]
fn set_private_file_options(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(all(not(unix), not(target_arch = "wasm32"), feature = "native-net"))]
fn set_private_file_options(_options: &mut std::fs::OpenOptions) {}

#[cfg(all(unix, not(target_arch = "wasm32"), feature = "native-net"))]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "identity seed path has no parent",
        )
    })?;
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(all(not(unix), not(target_arch = "wasm32"), feature = "native-net"))]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [42u8; 32];

    #[test]
    fn capability_receiver_uses_the_persisted_identity_key() {
        let identity = WalkieIdentity::from_seed(SEED);
        assert_eq!(
            identity.capability_actor_id().0,
            *identity.capability_signing_key().verifying_key().as_bytes()
        );
        assert_eq!(
            identity.capability_actor_id().0,
            *identity.capability_signing_key().verifying_key().as_bytes()
        );
    }

    /// Capability receiver and transport endpoint are stable derivations of the
    /// persisted identity seed, while remaining distinct protocol roles.
    #[cfg(feature = "native-net")]
    #[test]
    fn capability_receiver_equals_iroh_endpoint_id_bytes() {
        let id = WalkieIdentity::from_seed(SEED);

        let receiver = id.capability_actor_id();
        let iroh_public: iroh::PublicKey = id.iroh_secret().public(); // EndpointId == PublicKey

        assert_eq!(
            &receiver.0,
            iroh_public.as_bytes(),
            "both roles must be stable derivations of the persisted seed"
        );
    }

    /// `load_or_create` mints a seed on first run, persists it, and reloads the *same*
    /// identity afterwards.
    #[test]
    fn load_or_create_persists_then_reloads_same_identity() {
        let store = MemorySeedStore::new();
        let first = WalkieIdentity::load_or_create(&store).expect("infallible store");
        let second = WalkieIdentity::load_or_create(&store).expect("infallible store");

        assert_eq!(first.seed(), second.seed());
        assert_eq!(first.capability_actor_id(), second.capability_actor_id());
    }

    /// A pre-seeded store yields a deterministic author id.
    #[test]
    fn preseeded_store_is_deterministic() {
        let store = MemorySeedStore::with_seed(SEED);
        let id = WalkieIdentity::load_or_create(&store).expect("infallible store");
        assert_eq!(
            id.capability_actor_id(),
            WalkieIdentity::from_seed(SEED).capability_actor_id()
        );
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
    #[test]
    fn file_seed_store_round_trips_exact_identity() {
        let directory = std::env::temp_dir().join(format!(
            "walkie-songie-identity-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = directory.join("identity.seed");
        let store = FileSeedStore::new(&path);
        assert_eq!(store.load().unwrap(), None);
        store.save(&SEED).unwrap();
        assert_eq!(store.load().unwrap(), Some(SEED));

        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "native-net"))]
    #[test]
    fn file_seed_store_never_overwrites_an_existing_identity() {
        let directory = std::env::temp_dir().join(format!(
            "walkie-songie-identity-no-clobber-{}",
            uuid::Uuid::new_v4()
        ));
        let path = directory.join("identity.seed");
        let store = FileSeedStore::new(&path);
        store.save(&SEED).unwrap();
        let error = store.save(&[99; 32]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(store.load().unwrap(), Some(SEED));

        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
