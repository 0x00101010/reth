//! [`StateStore`] backed by a Commonware QMDB `current::ordered::variable` database.
//!
//! QMDB is async and runs on its own Commonware tokio runtime, on a dedicated thread. Calls from
//! reth threads block on that runtime's handle (through `block_in_place` when the caller is
//! itself a tokio worker).
//!
//! Each staged block is one merkleized QMDB batch. Batches chain and fork like the engine's
//! in-memory tree; a child only holds weak links to its ancestors, so the store keeps every
//! unpersisted batch alive until it is persisted or superseded. Persisting applies the newest
//! batch of the chain (ancestors apply with it) and commits once, with `(number, hash)` of the
//! block as commit metadata.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Used only by the `qmdb-migrate` binary.
#[cfg(feature = "cli")]
use {
    clap as _, eyre as _, reth_db as _, reth_db_api as _, reth_libmdbx as _, reth_stages_types as _,
};

use alloy_eips::BlockNumHash;
use alloy_primitives::{BlockNumber, B256, U256};
use commonware_cryptography::{sha256::Digest, Sha256};
use commonware_parallel::Rayon;
use commonware_runtime::{
    buffer::paged::{page_size, CacheRef},
    tokio::{Config as RuntimeConfig, Context, Runner},
    Runner as _, Strategizer as _, Supervisor as _, BLOB_PAGE_SIZE,
};
use commonware_storage::{
    journal::contiguous::variable::Config as LogConfig,
    merkle::{full, mmb},
    qmdb::current::{
        batch::MerkleizedBatch,
        ordered::variable::{Config as DbConfig, Db, Update},
    },
    translator::EightCap,
};
use parking_lot::{Mutex, RwLock};
use reth_primitives_traits::Account;
use reth_state_store::{
    decode_account, encode_account, ensure_follows, storage_key, StateReader, StateStore,
    StateStoreError, ACCOUNT_LEN,
};
use reth_trie_common::HashedPostState;
use std::{
    collections::BTreeMap,
    future::Future,
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    sync::Arc,
    thread,
};

/// Bitmap chunk size in bytes (256 operations per chunk).
const CHUNK: usize = 32;

/// Encoded store value: a 72-byte account or a 32-byte slot.
pub type Value = Vec<u8>;
type QDb = Db<mmb::Family, Context, Digest, Value, Sha256, EightCap, CHUNK, Rayon>;
type Batch = Arc<MerkleizedBatch<mmb::Family, Digest, Update<Digest, Value>, CHUNK, Rayon>>;
type QDbConfig = DbConfig<EightCap, ((), (commonware_codec::RangeCfg<usize>, ())), Rayon>;

/// [`QmdbStateStore`] configuration.
#[derive(Debug, Clone)]
pub struct QmdbConfig {
    /// Directory holding the database.
    pub dir: PathBuf,
    /// Page cache size in 4 KiB pages.
    pub page_cache_pages: NonZeroUsize,
    /// Threads used for merkleization.
    pub threads: NonZeroUsize,
}

impl QmdbConfig {
    /// Defaults: 4 GiB page cache, 16 merkleization threads.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            page_cache_pages: NonZeroUsize::new(1 << 20).expect("non-zero"),
            threads: NonZeroUsize::new(16).expect("non-zero"),
        }
    }
}

struct Inner {
    runtime: tokio::runtime::Handle,
    db: RwLock<Option<QDb>>,
    /// Unpersisted batches by block number, kept alive for their descendants.
    live: Mutex<BTreeMap<BlockNumber, Vec<Batch>>>,
    tip: Mutex<Option<(BlockNumHash, B256)>>,
}

impl Inner {
    fn block_on<F: Future>(&self, f: F) -> F::Output {
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| self.runtime.block_on(f))
        } else {
            self.runtime.block_on(f)
        }
    }

    fn with_db<T>(
        &self,
        f: impl FnOnce(&QDb) -> Result<T, StateStoreError>,
    ) -> Result<T, StateStoreError> {
        let guard = self.db.read();
        f(guard.as_ref().ok_or_else(poisoned)?)
    }
}

/// QMDB-backed [`StateStore`].
#[derive(Clone)]
pub struct QmdbStateStore {
    inner: Arc<Inner>,
    _shutdown: Arc<ShutdownOnDrop>,
}

impl std::fmt::Debug for QmdbStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbStateStore").field("tip", &*self.inner.tip.lock()).finish()
    }
}

/// Stops the runtime thread when the last store handle is dropped.
struct ShutdownOnDrop(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.lock().take() {
            let _ = tx.send(());
        }
    }
}

impl QmdbStateStore {
    /// Opens (or creates) the database in `config.dir`.
    pub fn open(config: QmdbConfig) -> Result<Self, StateStoreError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        thread::Builder::new()
            .name("qmdb-runtime".into())
            .spawn(move || {
                let runtime_config =
                    RuntimeConfig::new().with_storage_directory(config.dir.clone());
                Runner::new(runtime_config).start(|ctx| async move {
                    let db = QDb::init(ctx.child("state"), db_config(&ctx, &config), None).await;
                    let _ = ready_tx.send(db.map(|db| (tokio::runtime::Handle::current(), db)));
                    let _ = shutdown_rx.await;
                })
            })
            .map_err(backend)?;
        let (runtime, db) = ready_rx.recv().map_err(backend)?.map_err(backend)?;

        let inner = Inner {
            runtime,
            db: RwLock::new(Some(db)),
            live: Mutex::default(),
            tip: Mutex::default(),
        };
        let tip = inner.with_db(|db| {
            let metadata = inner.block_on(db.get_metadata()).map_err(backend)?;
            Ok(metadata.map(|bytes| (decode_metadata(&bytes), root_of(db.root()))))
        })?;
        *inner.tip.lock() = tip;
        Ok(Self {
            inner: Arc::new(inner),
            _shutdown: Arc::new(ShutdownOnDrop(Mutex::new(Some(shutdown_tx)))),
        })
    }

    /// Bulk-loads `entries` (from [`account_entry`] and [`storage_entry`]) into an empty store and
    /// records `block` as its tip. Returns the root.
    ///
    /// The root depends on the input order and on chunking, so both are fixed: entries are
    /// written in the given order in chunks of `chunk_size`, each merkleized, applied and
    /// committed, followed by one empty batch that carries `block` as commit metadata.
    /// `progress` is called with the running entry count after each chunk.
    pub fn import(
        &self,
        entries: impl Iterator<Item = Result<(B256, Value), StateStoreError>>,
        chunk_size: usize,
        block: BlockNumHash,
        mut progress: impl FnMut(u64),
    ) -> Result<B256, StateStoreError> {
        if self.inner.tip.lock().is_some() {
            return Err(backend("import requires an empty store"));
        }
        let mut entries = entries.peekable();
        let mut count = 0u64;
        let mut guard = self.inner.db.write();
        while entries.peek().is_some() {
            let db = guard.as_ref().ok_or_else(poisoned)?;
            let mut batch = db.new_batch();
            for entry in entries.by_ref().take(chunk_size) {
                let (k, v) = entry?;
                batch = batch.write(key(k), Some(v));
                count += 1;
            }
            let merkleized = self.inner.block_on(batch.merkleize(db)).map_err(backend)?;
            self.apply_and_commit(&mut guard, merkleized)?;
            progress(count);
        }
        let db = guard.as_ref().ok_or_else(poisoned)?;
        let batch = db.new_batch().with_metadata(Some(encode_metadata(block)));
        let merkleized = self.inner.block_on(batch.merkleize(db)).map_err(backend)?;
        let root = self.apply_and_commit(&mut guard, merkleized)?;
        *self.inner.tip.lock() = Some((block, root));
        Ok(root)
    }

    fn apply_and_commit(
        &self,
        guard: &mut Option<QDb>,
        batch: Batch,
    ) -> Result<B256, StateStoreError> {
        let db = guard.take().ok_or_else(poisoned)?;
        let db = self.inner.block_on(db.apply_batch(batch)).map_err(backend)?;
        let db = self.inner.block_on(db.commit()).map_err(backend)?;
        let root = root_of(db.root());
        *guard = Some(db);
        Ok(root)
    }

    /// Writes a block's diff into a new batch on top of `parent` (or the database) and
    /// merkleizes it.
    fn build(
        &self,
        db: &QDb,
        parent: Option<&QmdbPending>,
        block: BlockNumHash,
        diff: &HashedPostState,
    ) -> Result<Batch, StateStoreError> {
        let mut batch = match parent {
            Some(parent) => parent.batch.new_batch::<Sha256>(),
            None => db.new_batch(),
        };
        for (hashed_address, account) in &diff.accounts {
            batch = batch.write(key(*hashed_address), account.map(|a| encode_account(&a).to_vec()));
        }
        for (hashed_address, storage) in &diff.storages {
            for (hashed_slot, value) in &storage.storage {
                let value = (!value.is_zero()).then(|| value.to_be_bytes::<32>().to_vec());
                batch = batch.write(key(storage_key(*hashed_address, *hashed_slot)), value);
            }
        }
        let batch = batch.with_metadata(Some(encode_metadata(block)));
        self.inner.block_on(batch.merkleize(db)).map_err(backend)
    }
}

/// A staged, unpersisted block of a [`QmdbStateStore`].
#[derive(Clone)]
pub struct QmdbPending {
    batch: Batch,
    block: BlockNumHash,
    root: B256,
}

impl std::fmt::Debug for QmdbPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbPending").field("block", &self.block).field("root", &self.root).finish()
    }
}

/// Reader of a [`QmdbStateStore`] at the persisted tip or a pending block.
#[derive(Clone)]
pub struct QmdbReader {
    inner: Arc<Inner>,
    at: Option<Batch>,
}

impl std::fmt::Debug for QmdbReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbReader").field("pending", &self.at.is_some()).finish()
    }
}

impl QmdbReader {
    fn get(&self, key: Digest) -> Result<Option<Value>, StateStoreError> {
        self.inner.with_db(|db| {
            let value = match &self.at {
                None => self.inner.block_on(db.get(&key)),
                Some(batch) => self.inner.block_on(batch.new_batch::<Sha256>().get(&key, db)),
            };
            value.map_err(backend)
        })
    }
}

impl StateReader for QmdbReader {
    fn account(&self, hashed_address: B256) -> Result<Option<Account>, StateStoreError> {
        self.get(key(hashed_address))?
            .map(|bytes| {
                let bytes: &[u8; ACCOUNT_LEN] = bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| backend(format!("account value of {} bytes", bytes.len())))?;
                Ok(decode_account(bytes))
            })
            .transpose()
    }

    fn storage(&self, hashed_address: B256, hashed_slot: B256) -> Result<U256, StateStoreError> {
        Ok(self
            .get(key(storage_key(hashed_address, hashed_slot)))?
            .map(|bytes| U256::from_be_slice(&bytes))
            .unwrap_or_default())
    }
}

impl StateStore for QmdbStateStore {
    type Pending = QmdbPending;
    type Reader = QmdbReader;

    fn reader(&self, at: Option<&QmdbPending>) -> Result<QmdbReader, StateStoreError> {
        Ok(QmdbReader { inner: self.inner.clone(), at: at.map(|p| p.batch.clone()) })
    }

    fn stage(
        &self,
        parent: Option<&QmdbPending>,
        block: BlockNumHash,
        diff: &HashedPostState,
    ) -> Result<(QmdbPending, B256), StateStoreError> {
        let parent_number = match parent {
            Some(parent) => Some(parent.block.number),
            None => self.inner.tip.lock().map(|(tip, _)| tip.number),
        };
        if let Some(parent_number) = parent_number {
            ensure_follows(parent_number, block.number)?;
        }
        let batch = self.inner.with_db(|db| self.build(db, parent, block, diff))?;
        let root = root_of(batch.root());
        self.inner.live.lock().entry(block.number).or_default().push(batch.clone());
        Ok((QmdbPending { batch, block, root }, root))
    }

    fn persist(&self, chain: Vec<QmdbPending>) -> Result<(), StateStoreError> {
        let Some(last) = chain.last() else { return Ok(()) };
        let mut expected = *self.inner.tip.lock();
        for pending in &chain {
            if let Some((tip, _)) = expected {
                ensure_follows(tip.number, pending.block.number)?;
            }
            expected = Some((pending.block, pending.root));
        }

        let root = self.apply_and_commit(&mut self.inner.db.write(), last.batch.clone())?;
        debug_assert_eq!(root, last.root);

        self.inner.live.lock().retain(|number, _| *number > last.block.number);
        *self.inner.tip.lock() = Some((last.block, last.root));
        Ok(())
    }

    fn tip(&self) -> Result<Option<(BlockNumHash, B256)>, StateStoreError> {
        Ok(*self.inner.tip.lock())
    }

    fn unwind(&self, to: BlockNumber) -> Result<(), StateStoreError> {
        match *self.inner.tip.lock() {
            Some((tip, _)) if tip.number == to => Ok(()),
            _ => Err(StateStoreError::UnwindUnavailable(to)),
        }
    }
}

fn db_config(ctx: &Context, config: &QmdbConfig) -> QDbConfig {
    const ITEMS_PER_BLOB: NonZeroU64 = NonZeroU64::new(10_000_000).expect("non-zero");
    const BUFFER: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    let page_cache = CacheRef::new(page_size(BLOB_PAGE_SIZE), config.page_cache_pages);
    DbConfig {
        merkle: full::Config {
            journal_partition: "qmdb-merkle-journal".into(),
            metadata_partition: "qmdb-merkle-metadata".into(),
            items_per_blob: ITEMS_PER_BLOB,
            write_buffer: BUFFER,
            replay_buffer: BUFFER,
            strategy: ctx.strategy(config.threads),
            page_cache: page_cache.clone(),
        },
        log: LogConfig {
            partition: "qmdb-log".into(),
            items_per_section: ITEMS_PER_BLOB,
            compression: None,
            codec_config: ((), ((..=ACCOUNT_LEN).into(), ())),
            page_cache,
            write_buffer: BUFFER,
            replay_buffer: BUFFER,
        },
        grafted_metadata_partition: "qmdb-grafted-metadata".into(),
        translator: EightCap,
        init_cache: NonZeroUsize::new(1 << 22),
        init_buffer: NonZeroUsize::new(1 << 21).expect("non-zero"),
        init_concurrency: (),
    }
}

/// Store entry of an account.
pub fn account_entry(hashed_address: B256, account: &Account) -> (B256, Value) {
    (hashed_address, encode_account(account).to_vec())
}

/// Store entry of a non-zero storage slot.
pub fn storage_entry(hashed_address: B256, hashed_slot: B256, value: U256) -> (B256, Value) {
    (storage_key(hashed_address, hashed_slot), value.to_be_bytes::<32>().to_vec())
}

fn key(hash: B256) -> Digest {
    Digest::from(hash.0)
}

fn root_of(digest: Digest) -> B256 {
    B256::from_slice(digest.as_ref())
}

fn encode_metadata(block: BlockNumHash) -> Value {
    let mut out = Vec::with_capacity(40);
    out.extend_from_slice(&block.number.to_be_bytes());
    out.extend_from_slice(block.hash.as_slice());
    out
}

fn decode_metadata(bytes: &[u8]) -> BlockNumHash {
    BlockNumHash::new(
        u64::from_be_bytes(bytes[..8].try_into().expect("8 bytes")),
        B256::from_slice(&bytes[8..40]),
    )
}

fn backend(error: impl Into<Box<dyn core::error::Error + Send + Sync>>) -> StateStoreError {
    StateStoreError::Backend(error.into())
}

fn poisoned() -> StateStoreError {
    backend("QMDB store poisoned by an earlier failed persist")
}

#[cfg(test)]
mod tests;
