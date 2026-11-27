//! Optional recording of state reads and writes during re-execution, for key hotness analysis.
//!
//! Keys are QMDB store keys: `keccak(address)` for accounts and
//! `keccak(keccak(address) || keccak(slot))` for storage. Each block runs on a fresh executor,
//! so every key it reads reaches the database once ("an access" = a block reading the key).
//!
//! Workers send each block's accesses to [`Hotness`], which applies blocks strictly in block
//! order, so the gap between consecutive accesses to a key is exact and every access can be
//! classified hot or cold by block timestamp. Keys are sampled 1 in `2^sample_shift` by key hash
//! (counts are scaled back up); per-block totals are not sampled.

use alloy_primitives::{keccak256, map::B256Map, Address, B256, U256};
use reth_db_api::{
    cursor::{DbCursorRO, DbDupCursorRO},
    tables::{HashedAccounts, HashedStorages},
    transaction::DbTx,
};
use reth_revm::{
    db::BundleState,
    state::{AccountInfo, Bytecode},
    Database,
};
use std::{collections::BTreeMap, fmt::Write as _, path::Path};

const DAY: u64 = 86_400;
/// Hot windows reported, in seconds.
const WINDOWS: [(&str, u64); 6] = [
    ("1h", 3_600),
    ("1d", DAY),
    ("7d", 7 * DAY),
    ("14d", 14 * DAY),
    ("30d", 30 * DAY),
    ("all", u64::MAX),
];
/// Gap buckets between consecutive accesses to a key (upper bounds, seconds).
const GAPS: [(&str, u64); 7] = [
    ("<=2s", 2),
    ("<=1m", 60),
    ("<=1h", 3_600),
    ("<=1d", DAY),
    ("<=7d", 7 * DAY),
    ("<=30d", 30 * DAY),
    (">30d", u64::MAX),
];

/// QMDB store key of a storage slot.
fn storage_key(hashed_address: B256, hashed_slot: B256) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(hashed_address.as_slice());
    buf[32..].copy_from_slice(hashed_slot.as_slice());
    keccak256(buf)
}

/// One key accessed by a block.
#[derive(Debug, Clone, Copy)]
pub(super) struct Access {
    key: B256,
    hashed_address: B256,
    hashed_slot: Option<B256>,
    address: Address,
    /// The key had a value when the block read it.
    present: bool,
    /// The block wrote the key.
    written: bool,
}

/// What one block accessed.
#[derive(Debug, Default)]
pub(super) struct BlockAccesses {
    block: u64,
    timestamp: u64,
    /// All database reads (accounts + slots), unsampled.
    reads: u32,
    /// Reads of absent keys (no account, zero slot), unsampled.
    absent_reads: u32,
    /// Bytecodes read.
    codes: Vec<B256>,
    /// Sampled keys read or written.
    keys: Vec<Access>,
}

/// Database wrapper that records reads when enabled.
#[derive(Debug)]
pub(super) struct RecordingDb<DB> {
    inner: DB,
    record: bool,
    reads: Vec<Access>,
    codes: Vec<B256>,
}

impl<DB> RecordingDb<DB> {
    pub(super) const fn new(inner: DB, record: bool) -> Self {
        Self { inner, record, reads: Vec::new(), codes: Vec::new() }
    }

    /// Drains this block's reads, adds the keys `bundle` wrote, and keeps the sampled keys.
    pub(super) fn take_block(
        &mut self,
        block: u64,
        timestamp: u64,
        bundle: &BundleState,
        sample_mask: u8,
    ) -> BlockAccesses {
        let sampled = |key: &B256| key[31] & sample_mask == 0;
        let mut out = BlockAccesses {
            block,
            timestamp,
            reads: self.reads.len() as u32,
            absent_reads: self.reads.iter().filter(|r| !r.present).count() as u32,
            codes: std::mem::take(&mut self.codes),
            keys: Vec::new(),
        };
        let mut keys: B256Map<Access> = B256Map::default();
        for read in self.reads.drain(..) {
            if sampled(&read.key) {
                keys.insert(read.key, read);
            }
        }
        for (address, account) in &bundle.state {
            let hashed_address = keccak256(address);
            let mut mark = |key: B256, hashed_slot: Option<B256>| {
                if sampled(&key) {
                    keys.entry(key)
                        .or_insert(Access {
                            key,
                            hashed_address,
                            hashed_slot,
                            address: *address,
                            present: true,
                            written: false,
                        })
                        .written = true;
                }
            };
            if account.is_info_changed() {
                mark(hashed_address, None);
            }
            for (slot, value) in &account.storage {
                if value.is_changed() {
                    let hashed_slot = keccak256(B256::from(*slot));
                    mark(storage_key(hashed_address, hashed_slot), Some(hashed_slot));
                }
            }
        }
        out.keys = keys.into_values().collect();
        out
    }
}

impl<DB: Database> Database for RecordingDb<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let info = self.inner.basic(address)?;
        if self.record {
            let hashed_address = keccak256(address);
            self.reads.push(Access {
                key: hashed_address,
                hashed_address,
                hashed_slot: None,
                address,
                present: info.is_some(),
                written: false,
            });
        }
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if self.record {
            self.codes.push(code_hash);
        }
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let value = self.inner.storage(address, index)?;
        if self.record {
            let hashed_address = keccak256(address);
            let hashed_slot = keccak256(B256::from(index));
            self.reads.push(Access {
                key: storage_key(hashed_address, hashed_slot),
                hashed_address,
                hashed_slot: Some(hashed_slot),
                address,
                present: !value.is_zero(),
                written: false,
            });
        }
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}

/// A sampled key's access history.
#[derive(Debug, Clone, Copy)]
struct KeyRec {
    last_access: u64,
    /// 0 if never written in the scan.
    last_write: u64,
    accesses: u32,
    hashed_address: B256,
    hashed_slot: Option<B256>,
    address: Address,
}

/// Per-day totals. Reads are unsampled; access classes are sampled.
#[derive(Debug, Default, Clone, Copy)]
struct Day {
    blocks: u64,
    reads: u64,
    absent_reads: u64,
    /// Accesses whose key was accessed within the window before (sampled).
    warm: u64,
    /// Accesses to existing keys not accessed within the window before (sampled).
    cold_present: u64,
    /// Accesses to absent keys not accessed within the window before (sampled).
    cold_absent: u64,
}

/// In-order aggregator of [`BlockAccesses`].
#[derive(Debug)]
pub(super) struct Hotness {
    window: u64,
    scale: u64,
    next_block: u64,
    pending: BTreeMap<u64, BlockAccesses>,
    first_ts: Option<u64>,
    last_ts: u64,
    keys: B256Map<KeyRec>,
    gaps: [u64; GAPS.len()],
    days: BTreeMap<u64, Day>,
    codes: B256Map<u32>,
    reads_per_block: Vec<u32>,
    /// Exact per-block cold counts (existing, absent), kept only when unsampled.
    cold_per_block: Option<Vec<(u32, u32)>>,
}

impl Hotness {
    pub(super) fn new(first_block: u64, window: u64, sample_shift: u32) -> Self {
        Self {
            window,
            scale: 1 << sample_shift,
            next_block: first_block,
            pending: BTreeMap::new(),
            first_ts: None,
            last_ts: 0,
            keys: B256Map::default(),
            gaps: [0; GAPS.len()],
            days: BTreeMap::new(),
            codes: B256Map::default(),
            reads_per_block: Vec::new(),
            cold_per_block: (sample_shift == 0).then(Vec::new),
        }
    }

    /// Queues a block and applies every block that is now next in order.
    pub(super) fn push(&mut self, block: BlockAccesses) {
        self.pending.insert(block.block, block);
        while let Some(block) = self.pending.remove(&self.next_block) {
            self.apply(block);
            self.next_block += 1;
        }
    }

    /// Blocks received out of order and not yet applied.
    pub(super) fn pending(&self) -> usize {
        self.pending.len()
    }

    fn apply(&mut self, b: BlockAccesses) {
        let ts = b.timestamp;
        let first_ts = *self.first_ts.get_or_insert(ts);
        self.last_ts = ts;
        let warmed_up = ts >= first_ts + self.window;
        let day = self.days.entry(ts / DAY).or_default();
        day.blocks += 1;
        day.reads += b.reads as u64;
        day.absent_reads += b.absent_reads as u64;
        self.reads_per_block.push(b.reads);
        for code in b.codes {
            *self.codes.entry(code).or_default() += 1;
        }
        let (mut cold_present, mut cold_absent) = (0u32, 0u32);
        for a in b.keys {
            let warm = match self.keys.get_mut(&a.key) {
                Some(rec) => {
                    let gap = ts - rec.last_access;
                    self.gaps[GAPS.iter().position(|(_, max)| gap <= *max).expect("unbounded")] +=
                        1;
                    rec.last_access = ts;
                    rec.accesses += 1;
                    if a.written {
                        rec.last_write = ts;
                    }
                    Some(gap <= self.window)
                }
                None => {
                    self.keys.insert(
                        a.key,
                        KeyRec {
                            last_access: ts,
                            last_write: if a.written { ts } else { 0 },
                            accesses: 1,
                            hashed_address: a.hashed_address,
                            hashed_slot: a.hashed_slot,
                            address: a.address,
                        },
                    );
                    // First access in the scan: cold once a full window of history is behind it.
                    warmed_up.then_some(false)
                }
            };
            match (warmed_up, warm) {
                (true, Some(true)) => day.warm += 1,
                (true, Some(false)) if a.present => {
                    day.cold_present += 1;
                    cold_present += 1;
                }
                (true, Some(false)) => {
                    day.cold_absent += 1;
                    cold_absent += 1;
                }
                _ => {}
            }
        }
        if warmed_up && let Some(per_block) = &mut self.cold_per_block {
            per_block.push((cold_present, cold_absent));
        }
    }

    /// Checks which sampled keys exist in `tx`'s current state and writes the report.
    pub(super) fn write_report(
        mut self,
        tx: &impl DbTx,
        blocks: (u64, u64),
        path: &Path,
    ) -> eyre::Result<()> {
        eyre::ensure!(
            self.pending.is_empty(),
            "{} blocks were never applied (first missing: {})",
            self.pending.len(),
            self.next_block
        );
        let s = self.scale;
        let total_accounts = tx.entries::<HashedAccounts>()? as u64;
        let total_slots = tx.entries::<HashedStorages>()? as u64;
        let total_keys = total_accounts + total_slots;

        let mut keys: Vec<KeyRec> = self.keys.drain().map(|(_, r)| r).collect();
        keys.sort_unstable_by_key(|r| (r.hashed_address, r.hashed_slot));
        let mut accounts = tx.cursor_read::<HashedAccounts>()?;
        let mut storage = tx.cursor_dup_read::<HashedStorages>()?;
        let mut exists = Vec::with_capacity(keys.len());
        for r in &keys {
            exists.push(match r.hashed_slot {
                None => accounts.seek_exact(r.hashed_address)?.is_some(),
                Some(slot) => storage
                    .seek_by_key_subkey(r.hashed_address, slot)?
                    .is_some_and(|e| e.key == slot && !e.value.is_zero()),
            });
        }

        let pct = |n: u64, d: u64| n as f64 * 100.0 / d.max(1) as f64;
        let tip = self.last_ts;
        let first = self.first_ts.unwrap_or(tip);
        let mut out = String::new();
        let w = &mut out;
        writeln!(w, "# State accesses (reads + writes), blocks {}..={}", blocks.0, blocks.1)?;
        writeln!(
            w,
            "span {:.1} days, tip ts {tip}, hot window {}s, key sample 1/{s}",
            (tip - first) as f64 / DAY as f64,
            self.window
        )?;
        writeln!(
            w,
            "total keys at tip: {total_keys} (accounts {total_accounts}, storage {total_slots})"
        )?;

        writeln!(
            w,
            "\n## hot keys: accessed (read or written) within the window of the tip, existing"
        )?;
        writeln!(
            w,
            "window | keys | % of all | accounts (%) | storage (%) | of which written in window"
        )?;
        for (name, win) in WINDOWS {
            let cutoff = tip.saturating_sub(win);
            let (mut acc, mut sto, mut written) = (0u64, 0u64, 0u64);
            for (r, alive) in keys.iter().zip(&exists) {
                if *alive && r.last_access >= cutoff {
                    if r.hashed_slot.is_some() {
                        sto += 1
                    } else {
                        acc += 1
                    }
                    written += (r.last_write != 0 && r.last_write >= cutoff) as u64;
                }
            }
            let (acc, sto, written) = (acc * s, sto * s, written * s);
            writeln!(
                w,
                "{name} | {} | {:.3}% | {acc} ({:.3}%) | {sto} ({:.3}%) | {written} ({:.1}%)",
                acc + sto,
                pct(acc + sto, total_keys),
                pct(acc, total_accounts),
                pct(sto, total_slots),
                pct(written, acc + sto)
            )?;
        }
        let accessed = keys.len() as u64 * s;
        let alive = exists.iter().filter(|e| **e).count() as u64 * s;
        writeln!(w, "keys accessed in the scan: {accessed} ({alive} exist at the tip)")?;

        writeln!(w, "\n## gap between consecutive accesses to a key (sampled, scaled)")?;
        let repeats: u64 = self.gaps.iter().sum();
        for (i, (name, _)) in GAPS.iter().enumerate() {
            writeln!(w, "{name} | {} | {:.2}%", self.gaps[i] * s, pct(self.gaps[i], repeats))?;
        }

        let total: Day = self.days.values().fold(Day::default(), |mut t, d| {
            t.blocks += d.blocks;
            t.reads += d.reads;
            t.absent_reads += d.absent_reads;
            t.warm += d.warm;
            t.cold_present += d.cold_present;
            t.cold_absent += d.cold_absent;
            t
        });
        writeln!(w, "\n## reads")?;
        writeln!(
            w,
            "database reads (accounts + slots): {} over {} blocks, {:.0} per block; absent keys {:.1}%",
            total.reads,
            total.blocks,
            total.reads as f64 / total.blocks.max(1) as f64,
            pct(total.absent_reads, total.reads)
        )?;
        self.reads_per_block.sort_unstable();
        let q = |v: &[u32], p: f64| {
            v.get(((v.len().max(1) - 1) as f64 * p) as usize).copied().unwrap_or(0)
        };
        let rpb = &self.reads_per_block;
        writeln!(
            w,
            "reads per block p50 {} p90 {} p99 {} max {}",
            q(rpb, 0.5),
            q(rpb, 0.9),
            q(rpb, 0.99),
            q(rpb, 1.0)
        )?;
        let code_reads: u64 = self.codes.values().map(|n| *n as u64).sum();
        writeln!(w, "bytecode reads: {code_reads}, distinct bytecodes: {}", self.codes.len())?;

        writeln!(
            w,
            "\n## accesses under the {}s rule (after one window of warm-up; sampled, scaled)",
            self.window
        )?;
        let classified = total.warm + total.cold_present + total.cold_absent;
        writeln!(
            w,
            "warm {} ({:.2}%) | cold, key exists {} ({:.2}%) | cold, key absent {} ({:.2}%)",
            total.warm * s,
            pct(total.warm, classified),
            total.cold_present * s,
            pct(total.cold_present, classified),
            total.cold_absent * s,
            pct(total.cold_absent, classified)
        )?;
        if let Some(mut per_block) = self.cold_per_block.take() {
            let n = per_block.len().max(1) as f64;
            let mut present: Vec<u32> = per_block.iter().map(|c| c.0).collect();
            let mut both: Vec<u32> = per_block.iter().map(|c| c.0 + c.1).collect();
            per_block.clear();
            present.sort_unstable();
            both.sort_unstable();
            writeln!(
                w,
                "cold existing per block: mean {:.2} p50 {} p90 {} p99 {} max {}",
                total.cold_present as f64 / n,
                q(&present, 0.5),
                q(&present, 0.9),
                q(&present, 0.99),
                q(&present, 1.0)
            )?;
            writeln!(
                w,
                "all cold per block: p50 {} p90 {} p99 {} max {}",
                q(&both, 0.5),
                q(&both, 0.9),
                q(&both, 0.99),
                q(&both, 1.0)
            )?;
        }

        writeln!(w, "\n## per day (reads unsampled; cold counts sampled and scaled)")?;
        writeln!(w, "day | blocks | reads/block | absent % | cold existing/block | cold absent/block | cold existing % of accesses")?;
        for (day, d) in &self.days {
            let b = d.blocks.max(1) as f64;
            let classified = d.warm + d.cold_present + d.cold_absent;
            writeln!(
                w,
                "{day} | {} | {:.0} | {:.1} | {:.2} | {:.2} | {:.2}",
                d.blocks,
                d.reads as f64 / b,
                pct(d.absent_reads, d.reads),
                (d.cold_present * s) as f64 / b,
                (d.cold_absent * s) as f64 / b,
                pct(d.cold_present, classified)
            )?;
        }

        writeln!(w, "\n## contracts with the most hot storage slots (1d, scaled)")?;
        let cutoff = tip.saturating_sub(DAY);
        let mut per_contract: B256Map<(Address, u64, u64)> = B256Map::default();
        for (r, alive) in keys.iter().zip(&exists) {
            if *alive && r.hashed_slot.is_some() && r.last_access >= cutoff {
                let e = per_contract.entry(r.hashed_address).or_insert((r.address, 0, 0));
                e.1 += 1;
                e.2 += (r.last_write < cutoff) as u64;
            }
        }
        let mut top: Vec<_> = per_contract.into_values().collect();
        top.sort_unstable_by_key(|(_, slots, _)| std::cmp::Reverse(*slots));
        writeln!(w, "address | hot slots | not written in the day")?;
        for (address, slots, read_only) in top.into_iter().take(25) {
            writeln!(w, "{address} | {} | {}", slots * s, read_only * s)?;
        }

        std::fs::write(path, out)?;
        Ok(())
    }
}
