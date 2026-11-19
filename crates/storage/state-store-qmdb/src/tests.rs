use super::*;
use reth_state_store::MemoryStateStore;
use reth_trie_common::HashedStorage;

fn block(number: u64) -> BlockNumHash {
    BlockNumHash::new(number, B256::with_last_byte(number as u8))
}

fn open(dir: &tempfile::TempDir) -> QmdbStateStore {
    let mut config = QmdbConfig::new(dir.path());
    config.page_cache_pages = NonZeroUsize::new(1024).unwrap();
    config.threads = NonZeroUsize::new(2).unwrap();
    QmdbStateStore::open(config).unwrap()
}

/// Deterministic pseudo-random diffs over a small key space, so keys get created, updated and
/// deleted across blocks.
fn random_diffs(blocks: u64, seed: u64) -> Vec<HashedPostState> {
    let mut x = seed;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    (0..blocks)
        .map(|_| {
            let mut diff = HashedPostState::default();
            for _ in 0..8 {
                let addr = B256::with_last_byte((next() % 16) as u8);
                let account = (next() % 4 != 0).then(|| Account {
                    nonce: next() % 100,
                    balance: U256::from(next()),
                    bytecode_hash: (next() % 2 == 0).then(|| B256::with_last_byte(7)),
                });
                diff.accounts.insert(addr, account);
            }
            for _ in 0..16 {
                let addr = B256::with_last_byte((next() % 8) as u8);
                let slot = B256::with_last_byte((next() % 8) as u8);
                let value = if next() % 3 == 0 { U256::ZERO } else { U256::from(next()) };
                diff.storages
                    .entry(addr)
                    .or_insert_with(HashedStorage::default)
                    .storage
                    .insert(slot, value);
            }
            diff
        })
        .collect()
}

fn assert_same_reads(qmdb: &impl StateReader, reference: &impl StateReader) {
    for a in 0..16u8 {
        let addr = B256::with_last_byte(a);
        assert_eq!(qmdb.account(addr).unwrap(), reference.account(addr).unwrap(), "account {a}");
        for s in 0..8u8 {
            let slot = B256::with_last_byte(s);
            assert_eq!(
                qmdb.storage(addr, slot).unwrap(),
                reference.storage(addr, slot).unwrap(),
                "slot {a}/{s}"
            );
        }
    }
}

#[test]
fn reads_match_reference_through_pending_and_persisted_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let qmdb = open(&dir);
    let reference = MemoryStateStore::default();

    let mut q_parent: Option<QmdbPending> = None;
    let mut r_parent = None;
    let mut unpersisted = Vec::new();
    for (i, diff) in random_diffs(40, 1).iter().enumerate() {
        let b = block(i as u64 + 1);
        let (q, _) = qmdb.stage(q_parent.as_ref(), b, diff).unwrap();
        let (r, _) = reference.stage(r_parent.as_ref(), b, diff).unwrap();
        assert_same_reads(&qmdb.reader(Some(&q)).unwrap(), &reference.reader(Some(&r)).unwrap());
        unpersisted.push(q.clone());
        // Persist every 10 blocks, like the engine's persistence threshold.
        if (i + 1) % 10 == 0 {
            qmdb.persist(std::mem::take(&mut unpersisted)).unwrap();
            assert_eq!(qmdb.tip().unwrap(), Some((b, q.root)));
            assert_same_reads(&qmdb.reader(None).unwrap(), &reference.reader(Some(&r)).unwrap());
        }
        q_parent = Some(q);
        r_parent = Some(r);
    }
}

#[test]
fn roots_are_deterministic_and_survive_reopen() {
    let diffs = random_diffs(12, 2);
    let run = |dir: &tempfile::TempDir| {
        let store = open(dir);
        let mut parent: Option<QmdbPending> = None;
        let mut roots = Vec::new();
        for (i, diff) in diffs.iter().enumerate() {
            let (pending, root) = store.stage(parent.as_ref(), block(i as u64 + 1), diff).unwrap();
            roots.push(root);
            parent = Some(pending);
        }
        store.persist(vec![parent.unwrap()]).unwrap();
        roots
    };
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let roots = run(&dir_a);
    assert_eq!(roots, run(&dir_b));

    let reopened = open(&dir_a);
    assert_eq!(reopened.tip().unwrap(), Some((block(12), *roots.last().unwrap())));
}

#[test]
fn siblings_and_deletes_of_absent_keys() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let addr = B256::with_last_byte(1);
    let mut set = HashedPostState::default();
    set.accounts.insert(addr, Some(Account::default()));
    let (b1, _) = store.stage(None, block(1), &set).unwrap();

    let mut left = HashedPostState::default();
    left.accounts.insert(B256::with_last_byte(2), Some(Account { nonce: 1, ..Default::default() }));
    let mut right = HashedPostState::default();
    // Deletes of keys that never existed must be accepted.
    right.accounts.insert(B256::with_last_byte(3), None);
    right
        .storages
        .entry(B256::with_last_byte(3))
        .or_insert_with(HashedStorage::default)
        .storage
        .insert(B256::with_last_byte(9), U256::ZERO);

    let (l, l_root) = store.stage(Some(&b1), block(2), &left).unwrap();
    let (r, r_root) = store.stage(Some(&b1), block(2), &right).unwrap();
    assert_ne!(l_root, r_root);
    assert!(store.reader(Some(&r)).unwrap().account(B256::with_last_byte(2)).unwrap().is_none());

    store.persist(vec![b1, l]).unwrap();
    assert_eq!(store.tip().unwrap(), Some((block(2), l_root)));
    assert_eq!(
        store.reader(None).unwrap().account(B256::with_last_byte(2)).unwrap().map(|a| a.nonce),
        Some(1)
    );
}

#[test]
fn import_is_deterministic_and_readable() {
    let entries = || {
        (0..2_500u64).map(|i| {
            let addr = B256::left_padding_from(&i.to_be_bytes());
            Ok(if i % 2 == 0 {
                account_entry(addr, &Account { nonce: i, ..Default::default() })
            } else {
                storage_entry(addr, B256::with_last_byte(1), U256::from(i))
            })
        })
    };
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (open(&dir_a), open(&dir_b));
    let mut chunks = 0;
    let root_a = a.import(entries(), 1_000, block(7), |_| chunks += 1).unwrap();
    let root_b = b.import(entries(), 1_000, block(7), |_| {}).unwrap();
    assert_eq!(chunks, 3);
    assert_eq!(root_a, root_b);
    assert_ne!(
        root_a,
        open(&tempfile::tempdir().unwrap()).import(entries(), 999, block(7), |_| {}).unwrap()
    );

    let reader = a.reader(None).unwrap();
    assert_eq!(
        reader.account(B256::left_padding_from(&4u64.to_be_bytes())).unwrap().unwrap().nonce,
        4
    );
    assert_eq!(
        reader
            .storage(B256::left_padding_from(&5u64.to_be_bytes()), B256::with_last_byte(1))
            .unwrap(),
        U256::from(5)
    );
    drop((a, reader));
    assert_eq!(open(&dir_a).tip().unwrap(), Some((block(7), root_a)));

    // Blocks stage on top of the imported tip.
    let b8 = b.stage(None, block(8), &HashedPostState::default()).unwrap().0;
    b.persist(vec![b8]).unwrap();
}
