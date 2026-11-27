//! Write-recency (hot/cold) analysis of a storage-v2 datadir, from its changesets.
//!
//! A key is hot if it was accessed within `--window` seconds of the tip block's timestamp.
//! Changesets only record writes, so this measures the write half of that rule; read-only hot
//! keys need execution traces. Keys are QMDB store keys (`keccak(address)` for accounts,
//! `keccak(keccak(address) || keccak(slot))` for storage), sampled 1 in `2^sample_shift` by key
//! hash, so counts are scaled back up. Time uses block timestamps, so the window stays correct
//! across block-time changes (2 s before Denim, 200 ms after).

use alloy_primitives::{keccak256, map::B256Map, Address, B256, U256};
use clap::Parser;
use rayon::prelude::*;
use reth_db::{mdbx::DatabaseArguments, open_db_read_only, ClientVersion, Database};
use reth_db_api::{
    cursor::{DbCursorRO, DbDupCursorRO},
    tables::{HashedAccounts, HashedStorages},
    transaction::DbTx,
};
use reth_ethereum_primitives::EthPrimitives;
use reth_libmdbx::MaxReadTransactionDuration;
use reth_provider::{providers::StaticFileProvider, HeaderProvider};
use reth_state_store::storage_key;
use reth_storage_api::{ChangeSetReader, StorageChangeSetReader};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

const DAY: u64 = 86_400;
/// Windows reported, in seconds.
const WINDOWS: [(&str, u64); 5] =
    [("1h", 3_600), ("1d", DAY), ("7d", 7 * DAY), ("14d", 14 * DAY), ("all", u64::MAX)];
/// Reuse-gap buckets (upper bounds, seconds) for writes to a key already written in the scan.
const GAPS: [(&str, u64); 6] = [
    ("<=2s", 2),
    ("<=1m", 60),
    ("<=1h", 3_600),
    ("<=1d", DAY),
    ("<=7d", 7 * DAY),
    (">7d", u64::MAX),
];

#[derive(Debug, Parser)]
struct Args {
    /// reth datadir (contains `db/` and `static_files/`).
    #[arg(long)]
    datadir: PathBuf,
    /// Hot window, seconds before the tip's timestamp.
    #[arg(long, default_value_t = DAY)]
    window: u64,
    /// Track 1 in 2^N keys (by key hash).
    #[arg(long, default_value_t = 4)]
    sample_shift: u32,
    /// First block to scan (default: first block with changesets).
    #[arg(long)]
    from: Option<u64>,
    /// Last block to scan (default: the static files' highest block).
    #[arg(long)]
    to: Option<u64>,
    /// Blocks per parallel range.
    #[arg(long, default_value_t = 100_000)]
    range: u64,
}

/// A sampled key's history within the scan.
#[derive(Debug, Clone, Copy)]
struct Rec {
    first_ts: u64,
    last_ts: u64,
    writes: u32,
    /// The key's first write in the scan created it (it did not exist before).
    first_is_create: bool,
    /// Hashed address; for storage also the hashed slot (for the existence check).
    hashed_address: B256,
    hashed_slot: Option<B256>,
}

/// Unsampled per-day totals.
#[derive(Debug, Default, Clone, Copy)]
struct Day {
    blocks: u64,
    account_writes: u64,
    storage_writes: u64,
    /// Writes to keys that did not exist before the block (creations).
    account_creates: u64,
    storage_creates: u64,
}

#[derive(Debug, Default)]
struct Partial {
    keys: B256Map<Rec>,
    days: BTreeMap<u64, Day>,
    /// Sampled reuse gaps within the range, by bucket.
    gaps: [u64; GAPS.len()],
    /// Addresses of sampled storage keys, for naming hot contracts.
    addresses: B256Map<Address>,
    min_interval: u64,
}

fn bucket(gap: u64) -> usize {
    GAPS.iter().position(|(_, max)| gap <= *max).expect("last bucket is unbounded")
}

fn sampled(key: &B256, mask: u8) -> bool {
    key[31] & mask == 0
}

fn scan(
    sf: &StaticFileProvider<EthPrimitives>,
    blocks: std::ops::RangeInclusive<u64>,
    mask: u8,
) -> eyre::Result<Partial> {
    let mut out = Partial { min_interval: u64::MAX, ..Default::default() };
    let mut prev_ts = None;
    for block in blocks {
        let ts = sf
            .header_by_number(block)?
            .ok_or_else(|| eyre::eyre!("missing header {block}"))?
            .timestamp;
        if let Some(prev) = prev_ts {
            out.min_interval = out.min_interval.min(ts.saturating_sub(prev));
        }
        prev_ts = Some(ts);
        let day = out.days.entry(ts / DAY).or_default();
        day.blocks += 1;

        let mut touch =
            |key: B256, hashed_address: B256, hashed_slot: Option<B256>, create: bool| {
                if !sampled(&key, mask) {
                    return;
                }
                out.keys
                    .entry(key)
                    .and_modify(|rec| {
                        out.gaps[bucket(ts - rec.last_ts)] += 1;
                        rec.last_ts = ts;
                        rec.writes += 1;
                    })
                    .or_insert(Rec {
                        first_ts: ts,
                        last_ts: ts,
                        writes: 1,
                        first_is_create: create,
                        hashed_address,
                        hashed_slot,
                    });
            };

        for change in sf.account_block_changeset(block)? {
            let day = out.days.get_mut(&(ts / DAY)).expect("inserted above");
            day.account_writes += 1;
            day.account_creates += change.info.is_none() as u64;
            let hashed_address = keccak256(change.address);
            touch(hashed_address, hashed_address, None, change.info.is_none());
        }
        for (address, entry) in sf.storage_changeset(block)? {
            let day = out.days.get_mut(&(ts / DAY)).expect("inserted above");
            day.storage_writes += 1;
            day.storage_creates += entry.value.is_zero() as u64;
            let hashed_address = keccak256(address.address());
            let hashed_slot = keccak256(entry.key);
            let key = storage_key(hashed_address, hashed_slot);
            if sampled(&key, mask) {
                out.addresses.entry(hashed_address).or_insert_with(|| address.address());
            }
            touch(key, hashed_address, Some(hashed_slot), entry.value.is_zero());
        }
    }
    Ok(out)
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let mask = ((1u32 << args.sample_shift) - 1) as u8;
    let scale = 1u64 << args.sample_shift;
    let started = Instant::now();

    let sf = StaticFileProvider::<EthPrimitives>::read_only(args.datadir.join("static_files"))?;
    let tip = match args.to {
        Some(to) => to,
        None => sf
            .get_highest_static_file_block(reth_static_file_types::StaticFileSegment::Headers)
            .ok_or_else(|| eyre::eyre!("no headers"))?,
    };
    let from = match args.from {
        Some(from) => from,
        None => sf
            .get_lowest_range_start(reth_static_file_types::StaticFileSegment::StorageChangeSets)
            .ok_or_else(|| eyre::eyre!("no storage changesets"))?,
    };
    let tip_ts =
        sf.header_by_number(tip)?.ok_or_else(|| eyre::eyre!("missing tip header"))?.timestamp;
    let from_ts =
        sf.header_by_number(from)?.ok_or_else(|| eyre::eyre!("missing first header"))?.timestamp;
    eprintln!(
        "scan blocks {from}..={tip} ({:.1} days), tip_ts={tip_ts}, sample 1/{scale}",
        (tip_ts - from_ts) as f64 / DAY as f64
    );

    // Scan ranges in parallel, then merge them in block order so cross-range gaps are exact.
    let ranges: Vec<_> = (from..=tip)
        .step_by(args.range as usize)
        .map(|start| start..=(start + args.range - 1).min(tip))
        .collect();
    let partials = ranges
        .par_iter()
        .map(|range| scan(&sf, range.clone(), mask))
        .collect::<eyre::Result<Vec<_>>>()?;
    eprintln!("scanned in {:.0}s", started.elapsed().as_secs_f64());

    let mut keys: B256Map<Rec> = B256Map::default();
    let mut days: BTreeMap<u64, Day> = BTreeMap::new();
    let mut gaps = [0u64; GAPS.len()];
    let mut addresses: B256Map<Address> = B256Map::default();
    let mut min_interval = u64::MAX;
    let warm_start = from_ts + args.window;
    for part in partials {
        min_interval = min_interval.min(part.min_interval);
        for (i, n) in part.gaps.iter().enumerate() {
            gaps[i] += n;
        }
        for (day, d) in part.days {
            let total = days.entry(day).or_default();
            total.blocks += d.blocks;
            total.account_writes += d.account_writes;
            total.storage_writes += d.storage_writes;
            total.account_creates += d.account_creates;
            total.storage_creates += d.storage_creates;
        }
        addresses.extend(part.addresses);
        for (key, rec) in part.keys {
            match keys.get_mut(&key) {
                Some(prev) => {
                    gaps[bucket(rec.first_ts - prev.last_ts)] += 1;
                    prev.last_ts = rec.last_ts;
                    prev.writes += rec.writes;
                }
                None => {
                    keys.insert(key, rec);
                }
            }
        }
    }
    // Writes under the window rule. A repeat write is warm if the key was written within the
    // window before it (by the gap histogram; `window` must be a bucket bound). A key's first
    // write after one window of history is cold, unless it created the key: new keys start hot.
    // Creations are reported on their own.
    let window_bucket = bucket(args.window);
    let warm_writes: u64 = gaps[..=window_bucket].iter().sum();
    let late_first = keys.values().filter(|r| r.first_ts >= warm_start);
    let (creates_after_warmup, cold_first) =
        late_first.fold(
            (0u64, 0u64),
            |(c, f), r| if r.first_is_create { (c + 1, f) } else { (c, f + 1) },
        );
    let cold_writes: u64 = gaps[window_bucket + 1..].iter().sum::<u64>() + cold_first;

    // Which written keys still exist (deleted keys are not data).
    let db = open_db_read_only(
        args.datadir.join("db"),
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded)),
    )?;
    let (total_accounts, total_slots) = {
        let tx = db.tx()?;
        (tx.entries::<HashedAccounts>()? as u64, tx.entries::<HashedStorages>()? as u64)
    };
    let mut recs: Vec<(B256, Rec)> = keys.into_iter().collect();
    recs.sort_unstable_by_key(|(_, r)| (r.hashed_address, r.hashed_slot));
    let exists: Vec<bool> = recs
        .par_chunks(1 << 16)
        .map(|chunk| -> eyre::Result<Vec<bool>> {
            let tx = db.tx()?;
            let mut accounts = tx.cursor_read::<HashedAccounts>()?;
            let mut storage = tx.cursor_dup_read::<HashedStorages>()?;
            chunk
                .iter()
                .map(|(_, r)| {
                    Ok(match r.hashed_slot {
                        None => accounts.seek_exact(r.hashed_address)?.is_some(),
                        Some(slot) => storage
                            .seek_by_key_subkey(r.hashed_address, slot)?
                            .is_some_and(|e| e.key == slot && e.value != U256::ZERO),
                    })
                })
                .collect()
        })
        .collect::<eyre::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    eprintln!("existence checked in {:.0}s", started.elapsed().as_secs_f64());

    // Report.
    let total = total_accounts + total_slots;
    let pct = |n: u64, d: u64| n as f64 * 100.0 / d.max(1) as f64;
    println!("# QMDB key hotness (writes only), tip ts {tip_ts}, sample 1/{scale}");
    println!("total keys: {total} (accounts {total_accounts}, storage {total_slots})");
    println!("min block interval seen: {min_interval}s (0 => sub-second blocks)");
    println!("\n## hot keys by window (written within window of tip and still existing)");
    println!("window | hot keys | % of all | accounts (% of accounts) | storage (% of storage)");
    for (name, w) in WINDOWS {
        let cutoff = tip_ts.saturating_sub(w);
        let (mut acc, mut sto) = (0u64, 0u64);
        for ((_, r), alive) in recs.iter().zip(&exists) {
            if *alive && r.last_ts >= cutoff {
                if r.hashed_slot.is_some() {
                    sto += 1
                } else {
                    acc += 1
                }
            }
        }
        let (acc, sto) = (acc * scale, sto * scale);
        println!(
            "{name} | {} | {:.3}% | {acc} ({:.3}%) | {sto} ({:.3}%)",
            acc + sto,
            pct(acc + sto, total),
            pct(acc, total_accounts),
            pct(sto, total_slots)
        );
    }
    let written_deleted = exists.iter().filter(|e| !**e).count() as u64 * scale;
    println!("keys written during the scan and since deleted: {written_deleted}");

    println!("\n## reuse gap between consecutive writes to the same key (sampled, scaled)");
    let repeats: u64 = gaps.iter().sum();
    for (i, (name, _)) in GAPS.iter().enumerate() {
        println!("{name} | {} | {:.2}%", gaps[i] * scale, pct(gaps[i], repeats));
    }
    println!(
        "\n## writes under the {}s rule (first writes counted after one window of warm-up)\n\
         warm (key already hot): {} | cold (pre-existing key, idle > window): {} | \
         cold share {:.3}% | first writes that created a key: {}",
        args.window,
        warm_writes * scale,
        cold_writes * scale,
        pct(cold_writes, warm_writes + cold_writes),
        creates_after_warmup * scale,
    );

    println!("\n## per day (unsampled)");
    println!("day | blocks | account writes | storage writes | account creates | storage creates");
    for (day, d) in &days {
        println!(
            "{day} | {} | {} | {} | {} | {}",
            d.blocks, d.account_writes, d.storage_writes, d.account_creates, d.storage_creates
        );
    }

    // Contracts with the most hot storage slots in the main window.
    let cutoff = tip_ts.saturating_sub(args.window);
    let mut per_contract: B256Map<(u64, u64)> = B256Map::default();
    for ((_, r), alive) in recs.iter().zip(&exists) {
        if r.hashed_slot.is_some() && *alive && r.last_ts >= cutoff {
            let e = per_contract.entry(r.hashed_address).or_default();
            e.0 += 1;
            e.1 += r.writes as u64;
        }
    }
    let mut top: Vec<_> = per_contract.into_iter().collect();
    top.sort_unstable_by_key(|(_, (slots, _))| std::cmp::Reverse(*slots));
    println!("\n## top contracts by hot storage slots ({}s window, scaled)", args.window);
    println!("address | hot slots | writes in scan");
    for (hashed, (slots, writes)) in top.into_iter().take(25) {
        let address = addresses.get(&hashed).map(|a| a.to_string()).unwrap_or_default();
        println!("{address} | {} | {}", slots * scale, writes * scale);
    }
    eprintln!("done in {:.0}s", started.elapsed().as_secs_f64());
    Ok(())
}
