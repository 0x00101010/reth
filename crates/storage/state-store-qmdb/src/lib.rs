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
    clap as _, eyre as _, rayon as _, reth_db as _, reth_db_api as _, reth_libmdbx as _,
    reth_stages_types as _,
};

use alloy_eips::BlockNumHash;
use alloy_primitives::{BlockNumber, B256, U256};
use checkpoints::{Checkpoint, Checkpoints};
use commonware_cryptography::{sha256::Digest, Sha256};
use commonware_parallel::Rayon;
use commonware_runtime::{
    buffer::paged::{page_size, CacheRef},
    tokio::{Config as RuntimeConfig, Context, Runner},
    Runner as _, Strategizer as _, Supervisor as _, BLOB_PAGE_SIZE,
};
use commonware_storage::{
    journal::contiguous::variable::Config as LogConfig,
    merkle::{full, mmb, Location},
    qmdb::current::{
        batch::MerkleizedBatch,
        ordered::variable::{
            partitioned::{Config as DbConfig, Db},
            Update,
        },
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
    time::{Duration, Instant},
};

mod checkpoints;

/// Operations per log and Merkle journal blob.
const ITEMS_PER_BLOB: NonZeroU64 = NonZeroU64::new(10_000_000).expect("non-zero");

/// Bitmap chunk size in bytes (256 operations per chunk).
const CHUNK: usize = 32;

/// Encoded store value: a 72-byte account or a 32-byte slot.
pub type Value = Vec<u8>;
/// Index partition prefix bytes: `2^(8*P)` partitions. P=3 (16.8M partitions) is the largest
/// Commonware supports; partitions spill from sorted arrays to `BTreeMap`s past ~511 entries,
/// i.e. ~8.5B keys in total at P=3.
const PARTITION_BYTES: usize = 3;

type QDb = Db<mmb::Family, Context, Digest, Value, Sha256, EightCap, PARTITION_BYTES, CHUNK, Rayon>;
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
    /// Blocks of history kept in the log (the rewind and historical-proof window); older
    /// inactive operations are pruned.
    pub retain_blocks: u64,
    /// Prune only once the prune point has advanced this many operations. Defaults to one log
    /// blob: pruning frees whole blobs.
    pub prune_step: u64,
}

impl QmdbConfig {
    /// Defaults: 4 GiB page cache, 16 merkleization threads, 14 days of 200 ms blocks.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            page_cache_pages: NonZeroUsize::new(1 << 20).expect("non-zero"),
            threads: NonZeroUsize::new(16).expect("non-zero"),
            retain_blocks: 14 * 24 * 60 * 60 * 5,
            prune_step: ITEMS_PER_BLOB.get(),
        }
    }
}

struct Inner {
    runtime: tokio::runtime::Handle,
    db: RwLock<Option<QDb>>,
    /// Unpersisted batches by block number, kept alive for their descendants.
    live: Mutex<BTreeMap<BlockNumber, Vec<Batch>>>,
    tip: Mutex<Option<(BlockNumHash, B256)>>,
    checkpoints: Mutex<Checkpoints>,
    /// Location the database was last pruned to (logically; see `maybe_prune`).
    pruned_to: Mutex<u64>,
    retain_blocks: u64,
    prune_step: u64,
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
        let checkpoints = Checkpoints::open(&config.dir).map_err(backend)?;
        let retain_blocks = config.retain_blocks;
        let prune_step = config.prune_step;
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

        let pruned_to = *db.bounds().start;
        let inner = Inner {
            runtime,
            db: RwLock::new(Some(db)),
            live: Mutex::default(),
            tip: Mutex::default(),
            checkpoints: Mutex::new(checkpoints),
            pruned_to: Mutex::new(pruned_to),
            retain_blocks,
            prune_step,
        };
        let tip = inner.with_db(|db| {
            let metadata = inner.block_on(db.get_metadata()).map_err(backend)?;
            Ok(metadata.map(|bytes| (decode_metadata(&bytes), root_of(db.root()))))
        })?;
        *inner.tip.lock() = tip;
        let store = Self {
            inner: Arc::new(inner),
            _shutdown: Arc::new(ShutdownOnDrop(Mutex::new(Some(shutdown_tx)))),
        };
        // A database created before checkpoints existed starts its window at its tip.
        let mut guard = store.inner.db.write();
        if let Some((tip, _)) = tip &&
            store.inner.checkpoints.lock().is_empty()
        {
            store.record_db_checkpoint(&guard, tip.number)?;
        }
        store.maybe_prune(&mut guard)?;
        drop(guard);
        Ok(store)
    }

    /// Records the database's current commit as `number`'s checkpoint.
    fn record_db_checkpoint(
        &self,
        guard: &Option<QDb>,
        number: u64,
    ) -> Result<(), StateStoreError> {
        let db = guard.as_ref().ok_or_else(poisoned)?;
        let checkpoint =
            Checkpoint { number, size: *db.bounds().end, sync_boundary: *db.sync_boundary() };
        self.inner.checkpoints.lock().append(&[checkpoint]).map_err(backend)
    }

    /// Prunes inactive operations older than the retention window, in steps of whole blobs.
    fn maybe_prune(&self, guard: &mut Option<QDb>) -> Result<(), StateStoreError> {
        let Some((tip, _)) = *self.inner.tip.lock() else { return Ok(()) };
        let Some(target) =
            self.inner.checkpoints.lock().prune_target(tip.number, self.inner.retain_blocks)
        else {
            return Ok(())
        };
        let mut pruned_to = self.inner.pruned_to.lock();
        if target.sync_boundary < *pruned_to + self.inner.prune_step {
            return Ok(())
        }
        let start = Instant::now();
        let db = guard.take().ok_or_else(poisoned)?;
        let db =
            self.inner.block_on(db.prune(Location::from(target.sync_boundary))).map_err(backend)?;
        let pruned_ops = target.sync_boundary - *pruned_to;
        // The log frees whole blobs, so its physical start can stay behind the logical prune point.
        *pruned_to = target.sync_boundary;
        *guard = Some(db);
        self.inner.checkpoints.lock().drop_before(target.number).map_err(backend)?;
        tracing::info!(
            target: "state_store::qmdb",
            tip = tip.number,
            prune_block = target.number,
            pruned_ops,
            pruned_to = *pruned_to,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "Pruned QMDB log"
        );
        Ok(())
    }

    /// Bulk-loads `chunks` of entries (from [`account_entry`] and [`storage_entry`]) into an empty
    /// store and records `block` as its tip. Returns the root.
    ///
    /// The root depends on the input order and on chunking, so both are fixed by the caller:
    /// written chunk by chunk, each chunk merkleized, applied and committed, followed by one empty
    /// batch that carries `block` as commit metadata. Order within a chunk does not matter (a
    /// batch sorts its writes); chunk boundaries do.
    /// `progress` is called with running totals after each chunk.
    pub fn import(
        &self,
        chunks: impl Iterator<Item = Result<Vec<(B256, Value)>, StateStoreError>>,
        block: BlockNumHash,
        mut progress: impl FnMut(&ImportProgress),
    ) -> Result<B256, StateStoreError> {
        if self.inner.tip.lock().is_some() {
            return Err(backend("import requires an empty store"));
        }
        let mut stats = ImportProgress::default();
        let mut guard = self.inner.db.write();
        let mut start = Instant::now();
        for chunk in chunks {
            let chunk = chunk?;
            let db = guard.as_ref().ok_or_else(poisoned)?;
            let mut batch = db.new_batch();
            stats.entries += chunk.len() as u64;
            for (k, v) in chunk {
                batch = batch.write(key(k), Some(v));
            }
            let read = Instant::now();
            let merkleized = self.inner.block_on(batch.merkleize(db)).map_err(backend)?;
            let merkleize = Instant::now();
            self.apply_and_commit(&mut guard, merkleized)?;
            stats.read += read - start;
            stats.merkleize += merkleize - read;
            stats.apply_commit += merkleize.elapsed();
            progress(&stats);
            start = Instant::now();
        }
        let db = guard.as_ref().ok_or_else(poisoned)?;
        let batch = db.new_batch().with_metadata(Some(encode_metadata(block)));
        let merkleized = self.inner.block_on(batch.merkleize(db)).map_err(backend)?;
        let root = self.apply_and_commit(&mut guard, merkleized)?;
        *self.inner.tip.lock() = Some((block, root));
        self.record_db_checkpoint(&guard, block.number)?;
        self.maybe_prune(&mut guard)?;
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

/// Running totals reported by [`QmdbStateStore::import`] after each chunk.
#[derive(Debug, Default, Clone)]
pub struct ImportProgress {
    /// Entries imported so far.
    pub entries: u64,
    /// Time spent waiting for the next chunk and staging it.
    pub read: Duration,
    /// Time spent merkleizing.
    pub merkleize: Duration,
    /// Time spent applying and committing.
    pub apply_commit: Duration,
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

        let mut guard = self.inner.db.write();
        let root = self.apply_and_commit(&mut guard, last.batch.clone())?;
        debug_assert_eq!(root, last.root);
        let checkpoints: Vec<_> = chain
            .iter()
            .map(|p| Checkpoint {
                number: p.block.number,
                size: *p.batch.bounds().tip.size,
                sync_boundary: *p.batch.sync_boundary(),
            })
            .collect();
        self.inner.checkpoints.lock().append(&checkpoints).map_err(backend)?;

        self.inner.live.lock().retain(|number, _| *number > last.block.number);
        *self.inner.tip.lock() = Some((last.block, last.root));
        self.maybe_prune(&mut guard)
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
        init_concurrency: config.threads,
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
