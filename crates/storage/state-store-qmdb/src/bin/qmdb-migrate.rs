//! Migrates a reth (storage v2) datadir's current state into a QMDB database.
//!
//! Reads `HashedAccounts`, then `HashedStorages`, from the datadir's MDBX in key order and
//! bulk-loads them with [`QmdbStateStore::import`]. The datadir is opened read-only.

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use clap::Parser;
use reth_db::{mdbx::DatabaseArguments, open_db_read_only, ClientVersion, Database};
use reth_db_api::{
    cursor::DbCursorRO,
    tables::{HashedAccounts, HashedStorages, StageCheckpoints},
    transaction::DbTx,
};
use reth_libmdbx::MaxReadTransactionDuration;
use reth_stages_types::StageId;
use reth_state_store::{StateStore, StateStoreError};
use reth_state_store_qmdb::{account_entry, storage_entry, QmdbConfig, QmdbStateStore};
use std::{num::NonZeroUsize, path::PathBuf, time::Instant};

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
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let db = open_db_read_only(
        args.datadir.join("db"),
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded)),
    )?;
    let tx = db.tx()?;
    let tip = tx
        .get::<StageCheckpoints>(StageId::Finish.to_string())?
        .ok_or_else(|| eyre::eyre!("no Finish stage checkpoint"))?
        .block_number;
    let accounts = tx.entries::<HashedAccounts>()?;
    let slots = tx.entries::<HashedStorages>()?;
    eprintln!("tip={tip} accounts={accounts} storage_entries={slots}");

    let mut config = QmdbConfig::new(&args.out);
    config.page_cache_pages = NonZeroUsize::new(args.page_cache_pages).expect("non-zero");
    config.threads = NonZeroUsize::new(args.threads).expect("non-zero");
    let store = QmdbStateStore::open(config)?;
    eyre::ensure!(store.tip()?.is_none(), "{} already holds a database", args.out.display());

    let mut account_cursor = tx.cursor_read::<HashedAccounts>()?;
    let mut storage_cursor = tx.cursor_read::<HashedStorages>()?;
    let entries = account_cursor
        .walk(None)?
        .map(|row| row.map(|(hashed_address, account)| account_entry(hashed_address, &account)))
        .chain(
            storage_cursor
                .walk(None)?
                .filter(|row| !matches!(row, Ok((_, entry)) if entry.value.is_zero()))
                .map(|row| {
                    row.map(|(hashed_address, entry)| {
                        storage_entry(hashed_address, entry.key, entry.value)
                    })
                }),
        )
        .map(|row| row.map_err(|error| StateStoreError::Backend(Box::new(error))));

    let total = (accounts + slots) as u64;
    let start = Instant::now();
    let mut last_report = 0u64;
    let root =
        store.import(entries, args.chunk_size, BlockNumHash::new(tip, args.block_hash), |n| {
            if n - last_report >= 10_000_000 || n == total {
                last_report = n;
                let secs = start.elapsed().as_secs_f64();
                eprintln!(
                    "imported={n} of={total} pct={:.1} elapsed_s={secs:.0} keys_per_s={:.0}",
                    n as f64 * 100.0 / total.max(1) as f64,
                    n as f64 / secs
                );
            }
        })?;
    println!(
        "done block={tip} root={root} entries={last_report} elapsed_s={:.0}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
