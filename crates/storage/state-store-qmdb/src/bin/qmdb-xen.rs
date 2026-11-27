//! Read-only footprint of XEN in the current state: storage slots under the XEN contracts, plus
//! the proxy accounts their minters deployed and the slots those proxies own.
//!
//! Proxies are found by grouping accounts by code hash: a code hash is a clone if its bytecode is
//! short and delegates (DELEGATECALL) to a PUSH20 address (EIP-1167 and similar). Clone families
//! count as XEN when their implementation is one of the given minters (or a minter's own
//! implementation), or when the implementation's bytecode embeds the XEN token address.

use alloy_primitives::{address, b256, keccak256, map::B256Map, Address, B256};
use clap::Parser;
use rayon::prelude::*;
use reth_db::{mdbx::DatabaseArguments, open_db_read_only, ClientVersion, Database};
use reth_db_api::{
    cursor::{DbCursorRO, DbDupCursorRO},
    tables::{Bytecodes, HashedAccounts, HashedStorages},
    transaction::DbTx,
};
use reth_libmdbx::MaxReadTransactionDuration;
use std::{collections::HashSet, path::PathBuf, time::Instant};

/// XEN token on Base (cbXEN).
const XEN: Address = address!("0xffcbF84650cE02DaFE96926B37a0ac5E34932fa5");
/// Contracts whose slots are counted, and minters whose clones are counted.
const CONTRACTS: [(&str, Address); 3] = [
    ("XEN token (cbXEN)", XEN),
    ("XENTorrent", address!("0x379002701BF6f2862e3dFdd1f96d3C5E1BF450B6")),
    ("CoinTool batch minter", address!("0x9Ec1C3DcF667f2035FB4CD2eB42A1566fd54d2B7")),
];
/// `keccak256("eip1967.proxy.implementation") - 1`.
const EIP1967_IMPL: B256 =
    b256!("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");
/// Clone families at least this large are listed (and checked for an embedded XEN address).
const LIST_MIN: u64 = 1_000;

#[derive(Debug, Parser)]
struct Args {
    /// Reth datadir (the `db` directory inside it is opened read-only).
    #[arg(long)]
    datadir: PathBuf,
    /// Threads for the account scan.
    #[arg(long, default_value_t = 16)]
    threads: usize,
}

/// The delegation target of a short delegating bytecode, if it is one.
fn clone_target(code: &[u8]) -> Option<Address> {
    if code.is_empty() || code.len() > 100 || !code.contains(&0xf4) {
        return None;
    }
    let i = code.windows(21).position(|w| w[0] == 0x73)?;
    Some(Address::from_slice(&code[i + 1..i + 21]))
}

fn contains(code: &[u8], needle: &Address) -> bool {
    code.windows(20).any(|w| w == needle.as_slice())
}

/// Bytecode of `address`: `None` if the account doesn't exist, empty if it has no code.
fn code_of(tx: &impl DbTx, address: &Address) -> eyre::Result<Option<Vec<u8>>> {
    let Some(account) = tx.get::<HashedAccounts>(keccak256(address))? else { return Ok(None) };
    let Some(hash) = account.bytecode_hash else { return Ok(Some(Vec::new())) };
    Ok(tx.get::<Bytecodes>(hash)?.map(|c| c.original_byte_slice().to_vec()))
}

/// Number of storage slots under a hashed address.
fn slots_of(tx: &impl DbTx, hashed: B256) -> eyre::Result<u64> {
    let mut cursor = tx.cursor_dup_read::<HashedStorages>()?;
    let mut n = 0;
    for entry in cursor.walk_dup(Some(hashed), None)? {
        entry?;
        n += 1;
    }
    Ok(n)
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let started = Instant::now();
    let db = open_db_read_only(
        args.datadir.join("db"),
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded)),
    )?;
    rayon::ThreadPoolBuilder::new().num_threads(args.threads).build_global()?;

    let tx = db.tx()?;
    let total_accounts = tx.entries::<HashedAccounts>()? as u64;
    let total_slots = tx.entries::<HashedStorages>()? as u64;
    let total_keys = total_accounts + total_slots;

    // Contracts: code present, slot counts, and the implementations behind them.
    let mut minters: HashSet<Address> = HashSet::new();
    let mut out = String::new();
    let mut contract_slots = 0u64;
    out += "## XEN contracts\ncontract | address | code bytes | slots | implementation\n";
    for (name, address) in CONTRACTS {
        let code = code_of(&tx, &address)?;
        let slots = slots_of(&tx, keccak256(address))?;
        contract_slots += slots;
        let mut implementation = clone_target(code.as_deref().unwrap_or_default());
        if implementation.is_none() {
            let mut storage = tx.cursor_dup_read::<HashedStorages>()?;
            implementation = storage
                .seek_by_key_subkey(keccak256(address), keccak256(EIP1967_IMPL))?
                .filter(|e| e.key == keccak256(EIP1967_IMPL) && !e.value.is_zero())
                .map(|e| Address::from_word(B256::from(e.value)));
        }
        minters.insert(address);
        minters.extend(implementation);
        out += &format!(
            "{name} | {address} | {} | {slots} | {}\n",
            code.as_ref().map_or_else(|| "no account".to_string(), |c| c.len().to_string()),
            implementation.map_or_else(|| "-".to_string(), |a| a.to_string())
        );
    }
    drop(tx);
    eprintln!("contracts done in {:.0}s", started.elapsed().as_secs_f64());

    // Pass over all accounts, split by the first byte of the hashed address.
    struct Part {
        per_code: B256Map<u64>,
        clones: B256Map<Address>,
        members: Vec<(B256, B256)>,
    }
    let parts = (0u16..256)
        .into_par_iter()
        .map(|prefix| -> eyre::Result<Part> {
            let tx = db.tx()?;
            let mut accounts = tx.cursor_read::<HashedAccounts>()?;
            let mut part = Part {
                per_code: B256Map::default(),
                clones: B256Map::default(),
                members: Vec::new(),
            };
            let mut seen: B256Map<Option<Address>> = B256Map::default();
            let mut start = B256::ZERO;
            start[0] = prefix as u8;
            for entry in accounts.walk(Some(start))? {
                let (hashed, account) = entry?;
                if hashed[0] as u16 != prefix {
                    break;
                }
                let Some(code_hash) = account.bytecode_hash else { continue };
                *part.per_code.entry(code_hash).or_default() += 1;
                let target = match seen.get(&code_hash) {
                    Some(t) => *t,
                    None => {
                        let t = tx
                            .get::<Bytecodes>(code_hash)?
                            .and_then(|c| clone_target(c.original_byte_slice()));
                        seen.insert(code_hash, t);
                        t
                    }
                };
                if let Some(t) = target {
                    part.clones.insert(code_hash, t);
                    part.members.push((code_hash, hashed));
                }
            }
            Ok(part)
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let mut per_code: B256Map<u64> = B256Map::default();
    let mut clones: B256Map<Address> = B256Map::default();
    for p in &parts {
        for (h, n) in &p.per_code {
            *per_code.entry(*h).or_default() += n;
        }
        clones.extend(p.clones.iter().map(|(h, a)| (*h, *a)));
    }
    let contracts_with_code: u64 = per_code.values().sum();
    eprintln!("account scan done in {:.0}s", started.elapsed().as_secs_f64());

    // Classify clone families.
    let tx = db.tx()?;
    let mut families: Vec<(B256, Address, u64)> =
        clones.iter().map(|(h, a)| (*h, *a, per_code[h])).collect();
    families.sort_unstable_by_key(|f| std::cmp::Reverse(f.2));
    let mut selected: B256Map<&str> = B256Map::default();
    out += &format!(
        "\n## clone families (short delegating bytecode) with >= {LIST_MIN} accounts\n\
         code hash | implementation | accounts | XEN? (why)\n"
    );
    for (hash, implementation, count) in &families {
        let why = if minters.contains(implementation) {
            Some("implementation is a listed minter")
        } else if *count >= LIST_MIN &&
            code_of(&tx, implementation)?.is_some_and(|c| contains(&c, &XEN))
        {
            Some("implementation bytecode embeds the cbXEN address")
        } else {
            None
        };
        if let Some(why) = why {
            selected.insert(*hash, why);
        }
        if *count >= LIST_MIN || why.is_some() {
            out += &format!("{hash} | {implementation} | {count} | {}\n", why.unwrap_or("no"));
        }
    }
    drop(tx);

    // Proxies and their slots.
    let proxies: Vec<B256> = parts
        .iter()
        .flat_map(|p| p.members.iter())
        .filter(|(h, _)| selected.contains_key(h))
        .map(|(_, a)| *a)
        .collect();
    let proxy_slot_counts = proxies
        .par_chunks(1 << 14)
        .map(|chunk| -> eyre::Result<(u64, u64)> {
            let tx = db.tx()?;
            let (mut slots, mut with_slots) = (0, 0);
            for hashed in chunk {
                let n = slots_of(&tx, *hashed)?;
                slots += n;
                with_slots += (n > 0) as u64;
            }
            Ok((slots, with_slots))
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let proxy_slots: u64 = proxy_slot_counts.iter().map(|c| c.0).sum();
    let proxies_with_slots: u64 = proxy_slot_counts.iter().map(|c| c.1).sum();
    let proxy_accounts = proxies.len() as u64;

    let pct = |n: u64, d: u64| n as f64 * 100.0 / d as f64;
    let total = contract_slots + proxy_accounts + proxy_slots;
    let all_slots = contract_slots + proxy_slots;
    let summary = format!(
        "# XEN footprint in current state\n\
         total keys {total_keys} (accounts {total_accounts}, slots {total_slots}); \
         accounts with code {contracts_with_code}, distinct code hashes {}\n\n\
         XEN contract slots: {contract_slots}\n\
         proxy accounts: {proxy_accounts} ({:.3}% of accounts; {proxies_with_slots} own slots)\n\
         proxy-owned slots: {proxy_slots}\n\
         total keys: {total} = {:.3}% of all keys\n\
         slots (contracts + proxies): {all_slots} = {:.3}% of slots\n\n",
        per_code.len(),
        pct(proxy_accounts, total_accounts),
        pct(total, total_keys),
        pct(all_slots, total_slots),
    );
    print!("{summary}{out}");
    eprintln!("done in {:.0}s", started.elapsed().as_secs_f64());
    Ok(())
}
