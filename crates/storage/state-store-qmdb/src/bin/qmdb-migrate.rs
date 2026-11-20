//! Migrates a reth (storage v2) datadir's current state into a QMDB database.
//!
//! Reads `HashedStorages`, then `HashedAccounts`, from the datadir's MDBX (opened read-only) in
//! key order and bulk-loads them with [`QmdbStateStore::import`] in chunks of `--chunk-size`
//! entries. Order and chunking define the resulting root.
//!
//! Storage goes first: its keys (`keccak(hashed_address || hashed_slot)`) are spread uniformly,
//! so the ordered index fills evenly. Accounts are sorted by key; loading them into a sparse index
//! makes every predecessor lookup walk back across empty partitions.
//!
//! A producer thread reads rows and builds chunks (keys and encodings computed in parallel)
//! while the store merkleizes and commits the previous chunk.

use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, U256};
use clap::Parser;
use rayon::prelude::*;
use reth_db::{mdbx::DatabaseArguments, open_db_read_only, ClientVersion, Database, DatabaseEnv};
use reth_db_api::{
    cursor::DbCursorRO,
    tables::{HashedAccounts, HashedStorages, StageCheckpoints},
    transaction::DbTx,
};
use reth_libmdbx::MaxReadTransactionDuration;
use reth_primitives_traits::Account;
use reth_stages_types::StageId;
use reth_state_store::{StateStore, StateStoreError};
use reth_state_store_qmdb::{account_entry, storage_entry, QmdbConfig, QmdbStateStore, Value};
use std::{num::NonZeroUsize, path::PathBuf, sync::mpsc, thread, time::Instant};

#[derive(Debug, Parser)]
struct Args {
    /// reth datadir (contains `db/`).
    #[arg(long)]
    datadir: PathBuf,
    /// Output directory for the QMDB database.
    #[arg(long)]
    out: PathBuf,
    /// Hash of the datadir's tip block (the `Finish` stage checkpoint).
    #[arg(long)]
    block_hash: B256,
    /// Entries per committed chunk. Part of the root's definition.
    #[arg(long, default_value_t = 1 << 20)]
    chunk_size: usize,
    /// Page cache size in 4 KiB pages.
    #[arg(long, default_value_t = 1 << 20)]
    page_cache_pages: usize,
    /// Merkleization threads.
    #[arg(long, default_value_t = 16)]
    threads: usize,
    /// Stop after this many entries (for measurement runs).
    #[arg(long)]
    limit: Option<usize>,
    /// Report progress every this many entries.
    #[arg(long, default_value_t = 10_000_000)]
    report_every: u64,
}

/// A row as read from MDBX, before key derivation and encoding.
enum Row {
    Storage(B256, B256, U256),
    Account(B256, Account),
}

impl Row {
    fn entry(&self) -> (B256, Value) {
        match self {
            Self::Storage(address, slot, value) => storage_entry(*address, *slot, *value),
            Self::Account(address, account) => account_entry(*address, account),
        }
    }
}

type Chunk = Result<Vec<(B256, Value)>, StateStoreError>;

fn backend(error: impl Into<Box<dyn core::error::Error + Send + Sync>>) -> StateStoreError {
    StateStoreError::Backend(error.into())
}

/// Reads rows in migration order and sends them as encoded chunks.
fn produce(
    db: &DatabaseEnv,
    chunk_size: usize,
    limit: usize,
    tx: &mpsc::SyncSender<Chunk>,
) -> Result<(), StateStoreError> {
    let read = db.tx().map_err(backend)?;
    let mut storage = read.cursor_read::<HashedStorages>().map_err(backend)?;
    let mut accounts = read.cursor_read::<HashedAccounts>().map_err(backend)?;
    let rows = storage
        .walk(None)
        .map_err(backend)?
        .filter(|row| !matches!(row, Ok((_, entry)) if entry.value.is_zero()))
        .map(|row| row.map(|(address, entry)| Row::Storage(address, entry.key, entry.value)))
        .chain(
            accounts
                .walk(None)
                .map_err(backend)?
                .map(|row| row.map(|(address, account)| Row::Account(address, account))),
        )
        .take(limit);

    let mut buf = Vec::with_capacity(chunk_size);
    let send = |buf: &mut Vec<Row>| {
        let chunk: Vec<_> = buf.par_iter().map(Row::entry).collect();
        buf.clear();
        tx.send(Ok(chunk)).is_ok()
    };
    for row in rows {
        buf.push(row.map_err(backend)?);
        if buf.len() == chunk_size && !send(&mut buf) {
            return Ok(())
        }
    }
    if !buf.is_empty() {
        send(&mut buf);
    }
    Ok(())
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let db = open_db_read_only(
        args.datadir.join("db"),
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded)),
    )?;
    let (tip, accounts, slots) = {
        let tx = db.tx()?;
        let tip = tx
            .get::<StageCheckpoints>(StageId::Finish.to_string())?
            .ok_or_else(|| eyre::eyre!("no Finish stage checkpoint"))?
            .block_number;
        (tip, tx.entries::<HashedAccounts>()?, tx.entries::<HashedStorages>()?)
    };
    eprintln!("tip={tip} accounts={accounts} storage_entries={slots}");

    let mut config = QmdbConfig::new(&args.out);
    config.page_cache_pages = NonZeroUsize::new(args.page_cache_pages).expect("non-zero");
    config.threads = NonZeroUsize::new(args.threads).expect("non-zero");
    let store = QmdbStateStore::open(config)?;
    eyre::ensure!(store.tip()?.is_none(), "{} already holds a database", args.out.display());

    let limit = args.limit.unwrap_or(usize::MAX);
    let total = limit.min(accounts + slots) as u64;
    let (chunk_tx, chunk_rx) = mpsc::sync_channel::<Chunk>(4);
    let chunk_size = args.chunk_size;
    let producer = thread::spawn(move || {
        if let Err(error) = produce(&db, chunk_size, limit, &chunk_tx) {
            let _ = chunk_tx.send(Err(error));
        }
    });

    let start = Instant::now();
    let mut last_report = 0u64;
    let root =
        store.import(chunk_rx.into_iter(), BlockNumHash::new(tip, args.block_hash), |p| {
            let n = p.entries;
            if n - last_report >= args.report_every || n == total {
                last_report = n;
                let secs = start.elapsed().as_secs_f64();
                eprintln!(
                    "imported={n} of={total} pct={:.2} elapsed_s={secs:.0} keys_per_s={:.0} \
                 wait_s={:.1} merkleize_s={:.1} apply_commit_s={:.1}",
                    n as f64 * 100.0 / total.max(1) as f64,
                    n as f64 / secs,
                    p.read.as_secs_f64(),
                    p.merkleize.as_secs_f64(),
                    p.apply_commit.as_secs_f64(),
                );
            }
        })?;
    producer.join().map_err(|_| eyre::eyre!("producer panicked"))?;

    let peak_rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).map(str::to_owned))
        .unwrap_or_default();
    println!(
        "done block={tip} root={root} entries={last_report} elapsed_s={:.0} {peak_rss}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
