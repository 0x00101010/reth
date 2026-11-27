//! Optional recording of state reads during re-execution, for key hotness analysis.
//!
//! Keys are QMDB store keys: `keccak(address)` for accounts and
//! `keccak(keccak(address) || keccak(slot))` for storage. With a fresh executor per block, every
//! key a block reads reaches the database once, so per-key counts are "blocks that read it".

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
use std::{fmt::Write as _, path::Path};

/// QMDB store key of a storage slot.
fn storage_key(hashed_address: B256, hashed_slot: B256) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(hashed_address.as_slice());
    buf[32..].copy_from_slice(hashed_slot.as_slice());
    keccak256(buf)
}

/// A state read: store key, hashed address, hashed slot (storage only), and whether the value
/// existed (account present, slot non-zero).
#[derive(Debug, Clone, Copy)]
struct Read {
    key: B256,
    hashed_address: B256,
    hashed_slot: Option<B256>,
    present: bool,
    address: Address,
}

/// Database wrapper that records reads when enabled.
#[derive(Debug)]
pub(super) struct RecordingDb<DB> {
    inner: DB,
    record: bool,
    reads: Vec<Read>,
    code_reads: Vec<B256>,
}

impl<DB> RecordingDb<DB> {
    pub(super) const fn new(inner: DB, record: bool) -> Self {
        Self { inner, record, reads: Vec::new(), code_reads: Vec::new() }
    }
}

impl<DB: Database> Database for RecordingDb<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let info = self.inner.basic(address)?;
        if self.record {
            let hashed_address = keccak256(address);
            self.reads.push(Read {
                key: hashed_address,
                hashed_address,
                hashed_slot: None,
                present: info.is_some(),
                address,
            });
        }
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if self.record {
            self.code_reads.push(code_hash);
        }
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let value = self.inner.storage(address, index)?;
        if self.record {
            let hashed_address = keccak256(address);
            let hashed_slot = keccak256(B256::from(index));
            self.reads.push(Read {
                key: storage_key(hashed_address, hashed_slot),
                hashed_address,
                hashed_slot: Some(hashed_slot),
                present: !value.is_zero(),
                address,
            });
        }
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}

/// Per-key totals.
#[derive(Debug, Clone, Copy)]
struct KeyStats {
    /// Blocks that read the key.
    blocks: u32,
    /// Blocks that wrote the key.
    writes: u32,
    /// Some read found a value.
    present_on_read: bool,
    hashed_address: B256,
    hashed_slot: Option<B256>,
    address: Address,
}

/// Per-worker (then merged) access statistics.
#[derive(Debug, Default)]
pub(super) struct Accesses {
    keys: B256Map<KeyStats>,
    codes: B256Map<u32>,
    /// Database reads per block (accounts + slots), one entry per recorded block.
    reads_per_block: Vec<u32>,
    absent_reads: u64,
    total_reads: u64,
    /// First block of the run; read events are stored as offsets from it.
    base_block: u64,
    /// Per key, the blocks that read it: offset from `base_block`, top bit set if the key
    /// existed when read.
    events: B256Map<Vec<u32>>,
}

const PRESENT_BIT: u32 = 1 << 31;

impl Accesses {
    pub(super) fn new(base_block: u64) -> Self {
        Self { base_block, ..Default::default() }
    }

    /// Takes the reads `block` recorded, plus the keys it wrote (from its bundle).
    pub(super) fn absorb<DB>(
        &mut self,
        db: &mut RecordingDb<DB>,
        bundle: &BundleState,
        block: u64,
    ) {
        if !db.record || (db.reads.is_empty() && bundle.state.is_empty()) {
            return;
        }
        self.reads_per_block.push(db.reads.len() as u32);
        for read in db.reads.drain(..) {
            self.total_reads += 1;
            self.absent_reads += !read.present as u64;
            let offset = (block - self.base_block) as u32;
            self.events
                .entry(read.key)
                .or_default()
                .push(offset | if read.present { PRESENT_BIT } else { 0 });
            let stats = self.keys.entry(read.key).or_insert(KeyStats {
                blocks: 0,
                writes: 0,
                present_on_read: false,
                hashed_address: read.hashed_address,
                hashed_slot: read.hashed_slot,
                address: read.address,
            });
            stats.blocks += 1;
            stats.present_on_read |= read.present;
        }
        for code in db.code_reads.drain(..) {
            *self.codes.entry(code).or_default() += 1;
        }
        for (address, account) in &bundle.state {
            let hashed_address = keccak256(address);
            let mut mark = |key: B256, hashed_slot: Option<B256>| {
                self.keys
                    .entry(key)
                    .or_insert(KeyStats {
                        blocks: 0,
                        writes: 0,
                        present_on_read: false,
                        hashed_address,
                        hashed_slot,
                        address: *address,
                    })
                    .writes += 1;
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
    }

    pub(super) fn merge(&mut self, other: Self) {
        for (key, s) in other.keys {
            match self.keys.get_mut(&key) {
                Some(t) => {
                    t.blocks += s.blocks;
                    t.writes += s.writes;
                    t.present_on_read |= s.present_on_read;
                }
                None => {
                    self.keys.insert(key, s);
                }
            }
        }
        for (code, n) in other.codes {
            *self.codes.entry(code).or_default() += n;
        }
        self.reads_per_block.extend(other.reads_per_block);
        self.absent_reads += other.absent_reads;
        self.total_reads += other.total_reads;
        for (key, blocks) in other.events {
            self.events.entry(key).or_default().extend(blocks);
        }
    }

    /// Checks which accessed keys exist in `tx`'s current state and writes the report.
    pub(super) fn write_report(
        mut self,
        tx: &impl DbTx,
        blocks: (u64, u64),
        window: u64,
        path: &Path,
    ) -> eyre::Result<()> {
        let total_accounts = tx.entries::<HashedAccounts>()? as u64;
        let total_slots = tx.entries::<HashedStorages>()? as u64;
        let total_keys = total_accounts + total_slots;

        let mut keys: Vec<(B256, KeyStats)> = self.keys.drain().collect();
        keys.sort_unstable_by_key(|(_, s)| (s.hashed_address, s.hashed_slot));
        let mut accounts = tx.cursor_read::<HashedAccounts>()?;
        let mut storage = tx.cursor_dup_read::<HashedStorages>()?;
        let mut exists = Vec::with_capacity(keys.len());
        for (_, s) in &keys {
            exists.push(match s.hashed_slot {
                None => accounts.seek_exact(s.hashed_address)?.is_some(),
                Some(slot) => storage
                    .seek_by_key_subkey(s.hashed_address, slot)?
                    .is_some_and(|e| e.key == slot && !e.value.is_zero()),
            });
        }

        let pct = |n: u64, d: u64| n as f64 * 100.0 / d.max(1) as f64;
        let mut out = String::new();
        let w = &mut out;
        writeln!(
            w,
            "# State reads and writes, blocks {}..={} (fresh executor per block)",
            blocks.0, blocks.1
        )?;
        writeln!(
            w,
            "total keys at tip: {total_keys} (accounts {total_accounts}, storage {total_slots})"
        )?;

        // Hot set: keys read or written in the window that exist at the tip.
        let (mut hot_acc, mut hot_sto, mut read_only, mut written) = (0u64, 0u64, 0u64, 0u64);
        let (mut accessed_acc, mut accessed_sto) = (0u64, 0u64);
        for ((_, s), alive) in keys.iter().zip(&exists) {
            let storage_key = s.hashed_slot.is_some();
            if storage_key {
                accessed_sto += 1
            } else {
                accessed_acc += 1
            }
            if !*alive {
                continue;
            }
            if storage_key {
                hot_sto += 1
            } else {
                hot_acc += 1
            }
            if s.writes > 0 {
                written += 1
            } else {
                read_only += 1
            }
        }
        writeln!(w, "\n## hot keys (read or written in the window, existing at the tip)")?;
        writeln!(
            w,
            "hot keys: {} = {:.3}% of all keys",
            hot_acc + hot_sto,
            pct(hot_acc + hot_sto, total_keys)
        )?;
        writeln!(w, "accounts: {hot_acc} = {:.3}% of accounts", pct(hot_acc, total_accounts))?;
        writeln!(w, "storage: {hot_sto} = {:.3}% of storage slots", pct(hot_sto, total_slots))?;
        writeln!(
            w,
            "of hot keys: written {written} ({:.1}%), read only {read_only} ({:.1}%)",
            pct(written, written + read_only),
            pct(read_only, written + read_only)
        )?;
        writeln!(w, "distinct keys accessed (incl. absent/deleted): accounts {accessed_acc}, storage {accessed_sto}")?;

        writeln!(w, "\n## reads")?;
        let blocks_n = self.reads_per_block.len() as u64;
        writeln!(
            w,
            "database reads (accounts + slots): {} over {blocks_n} blocks, {:.0} per block",
            self.total_reads,
            self.total_reads as f64 / blocks_n.max(1) as f64
        )?;
        writeln!(
            w,
            "reads of absent keys (no account / zero slot): {} ({:.1}%)",
            self.absent_reads,
            pct(self.absent_reads, self.total_reads)
        )?;
        self.reads_per_block.sort_unstable();
        let q = |p: f64| {
            self.reads_per_block.get(((blocks_n as f64 - 1.0) * p) as usize).copied().unwrap_or(0)
        };
        writeln!(
            w,
            "reads per block p50 {} p90 {} p99 {} max {}",
            q(0.5),
            q(0.9),
            q(0.99),
            q(1.0)
        )?;
        let code_reads: u64 = self.codes.values().map(|n| *n as u64).sum();
        writeln!(w, "bytecode reads: {code_reads}, distinct bytecodes: {}", self.codes.len())?;

        writeln!(w, "\n## how many blocks read each existing hot key")?;
        let buckets: [(&str, u32); 6] = [
            ("1", 1),
            ("2-10", 10),
            ("11-100", 100),
            ("101-1k", 1_000),
            ("1k-10k", 10_000),
            (">10k", u32::MAX),
        ];
        let mut counts = [0u64; 6];
        let mut reads_in = [0u64; 6];
        for ((_, s), alive) in keys.iter().zip(&exists) {
            if *alive && s.blocks > 0 {
                let i =
                    buckets.iter().position(|(_, max)| s.blocks <= *max).expect("unbounded last");
                counts[i] += 1;
                reads_in[i] += s.blocks as u64;
            }
        }
        let read_keys: u64 = counts.iter().sum();
        let read_total: u64 = reads_in.iter().sum();
        writeln!(w, "blocks reading key | keys (% of read keys) | reads (% of reads)")?;
        for (i, (name, _)) in buckets.iter().enumerate() {
            writeln!(
                w,
                "{name} | {} ({:.1}%) | {} ({:.1}%)",
                counts[i],
                pct(counts[i], read_keys),
                reads_in[i],
                pct(reads_in[i], read_total)
            )?;
        }

        writeln!(w, "\n## contracts with the most hot storage slots")?;
        let mut per_contract: B256Map<(Address, u64, u64, u64)> = B256Map::default();
        for ((_, s), alive) in keys.iter().zip(&exists) {
            if *alive && s.hashed_slot.is_some() {
                let e = per_contract.entry(s.hashed_address).or_insert((s.address, 0, 0, 0));
                e.1 += 1;
                e.2 += (s.writes == 0) as u64;
                e.3 += s.blocks as u64;
            }
        }
        let mut top: Vec<_> = per_contract.into_values().collect();
        top.sort_unstable_by_key(|(_, slots, _, _)| std::cmp::Reverse(*slots));
        writeln!(w, "address | hot slots | read-only slots | slot reads")?;
        for (address, slots, ro, reads) in top.into_iter().take(25) {
            writeln!(w, "{address} | {slots} | {ro} | {reads}")?;
        }

        // Cold reads under the window rule: a read is cold if the same key was not read in the
        // previous `window` blocks. Only blocks at least one window after the run's start are
        // evaluated, so every evaluated read has a full window of history behind it.
        let eval_start = blocks.0 + window;
        if eval_start < blocks.1 {
            let n = (blocks.1 - eval_start) as usize;
            let mut reads_eval = vec![0u32; n];
            let mut cold_present = vec![0u32; n];
            let mut cold_absent = vec![0u32; n];
            let mut promoted_keys = 0u64;
            for offsets in self.events.values_mut() {
                offsets.sort_unstable_by_key(|o| o & !PRESENT_BIT);
                let mut prev: Option<u64> = None;
                let mut promoted = false;
                for o in offsets.iter() {
                    let b = self.base_block + (o & !PRESENT_BIT) as u64;
                    if (eval_start..blocks.1).contains(&b) {
                        let i = (b - eval_start) as usize;
                        reads_eval[i] += 1;
                        if prev.is_none_or(|p| b - p > window) {
                            if o & PRESENT_BIT != 0 {
                                cold_present[i] += 1;
                                promoted = true;
                            } else {
                                cold_absent[i] += 1;
                            }
                        }
                    }
                    prev = Some(b);
                }
                promoted_keys += promoted as u64;
            }
            let sum = |v: &[u32]| v.iter().map(|x| *x as u64).sum::<u64>();
            let quantiles = |mut v: Vec<u32>| {
                v.sort_unstable();
                let q = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
                format!("p50 {} p90 {} p99 {} max {}", q(0.5), q(0.9), q(0.99), q(1.0))
            };
            let (total, cp, ca) = (sum(&reads_eval), sum(&cold_present), sum(&cold_absent));
            let per_block = |x: u64| x as f64 / n as f64;
            writeln!(w, "\n## cold reads: key not read in the previous {window} blocks (blocks {eval_start}..{})", blocks.1)?;
            writeln!(
                w,
                "reads evaluated: {total} over {n} blocks ({:.1} per block)",
                per_block(total)
            )?;
            writeln!(w, "cold reads of existing keys (disk reads): {cp} = {:.2}% of reads, {:.2} per block, per-block {}",
                pct(cp, total), per_block(cp), quantiles(cold_present.clone()))?;
            writeln!(w, "cold reads of absent keys (answered by a negative filter): {ca} = {:.2}% of reads, {:.2} per block, per-block {}",
                pct(ca, total), per_block(ca), quantiles(cold_absent.clone()))?;
            let both: Vec<u32> =
                cold_present.iter().zip(&cold_absent).map(|(a, b)| a + b).collect();
            writeln!(w, "all cold reads per block: {}", quantiles(both))?;
            writeln!(w, "distinct existing keys promoted cold -> hot: {promoted_keys}")?;
        }

        std::fs::write(path, out)?;
        Ok(())
    }
}
