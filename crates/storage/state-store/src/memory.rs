//! In-memory [`StateStore`], the reference that other implementations are tested against.

use crate::{
    encode_account, ensure_follows, storage_key, StateReader, StateStore, StateStoreError,
};
use alloy_eips::BlockNumHash;
use alloy_primitives::{keccak256, BlockNumber, B256, U256};
use parking_lot::RwLock;
use reth_primitives_traits::Account;
use reth_trie_common::HashedPostState;
use std::{collections::BTreeMap, sync::Arc};

/// Full state, keyed like the store: accounts by hashed address, slots by [`storage_key`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    accounts: BTreeMap<B256, Account>,
    storage: BTreeMap<B256, U256>,
}

impl State {
    fn apply(&mut self, diff: &HashedPostState) {
        for (hashed_address, account) in &diff.accounts {
            match account {
                Some(account) => self.accounts.insert(*hashed_address, *account),
                None => self.accounts.remove(hashed_address),
            };
        }
        for (hashed_address, storage) in &diff.storages {
            for (hashed_slot, value) in &storage.storage {
                let key = storage_key(*hashed_address, *hashed_slot);
                if value.is_zero() {
                    self.storage.remove(&key);
                } else {
                    self.storage.insert(key, *value);
                }
            }
        }
    }

    /// Commitment over the sorted entries. Not QMDB's root; only needs to be deterministic and
    /// sensitive to every entry.
    fn root(&self) -> B256 {
        let mut buf = Vec::with_capacity(self.accounts.len() * 105 + self.storage.len() * 65);
        for (key, account) in &self.accounts {
            buf.push(0);
            buf.extend_from_slice(key.as_slice());
            buf.extend_from_slice(&encode_account(account));
        }
        for (key, value) in &self.storage {
            buf.push(1);
            buf.extend_from_slice(key.as_slice());
            buf.extend_from_slice(&value.to_be_bytes::<32>());
        }
        keccak256(buf)
    }
}

/// A staged, unpersisted block of a [`MemoryStateStore`].
#[derive(Clone, Debug)]
pub struct MemoryPending(Arc<PendingBlock>);

#[derive(Debug)]
struct PendingBlock {
    block: BlockNumHash,
    root: B256,
    state: Arc<State>,
}

/// Reader of a [`MemoryStateStore`].
#[derive(Clone, Debug)]
pub struct MemoryReader(Arc<State>);

impl StateReader for MemoryReader {
    fn account(&self, hashed_address: B256) -> Result<Option<Account>, StateStoreError> {
        Ok(self.0.accounts.get(&hashed_address).copied())
    }

    fn storage(&self, hashed_address: B256, hashed_slot: B256) -> Result<U256, StateStoreError> {
        Ok(self
            .0
            .storage
            .get(&storage_key(hashed_address, hashed_slot))
            .copied()
            .unwrap_or_default())
    }
}

/// In-memory [`StateStore`]. Keeps every persisted state so it can unwind to any of them.
#[derive(Debug, Default)]
pub struct MemoryStateStore {
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Persisted states, oldest first; the last one is the tip.
    persisted: Vec<(BlockNumHash, B256, Arc<State>)>,
}

impl Inner {
    fn tip_state(&self) -> Arc<State> {
        self.persisted.last().map(|(_, _, state)| state.clone()).unwrap_or_default()
    }
}

impl MemoryStateStore {
    /// Store whose persisted tip is `block` with the given accounts and slots (by hashed key).
    pub fn with_genesis(block: BlockNumHash, diff: &HashedPostState) -> Self {
        let mut state = State::default();
        state.apply(diff);
        let root = state.root();
        Self { inner: RwLock::new(Inner { persisted: vec![(block, root, Arc::new(state))] }) }
    }
}

impl StateStore for MemoryStateStore {
    type Pending = MemoryPending;
    type Reader = MemoryReader;

    fn reader(&self, at: Option<&MemoryPending>) -> Result<MemoryReader, StateStoreError> {
        Ok(MemoryReader(match at {
            Some(pending) => pending.0.state.clone(),
            None => self.inner.read().tip_state(),
        }))
    }

    fn stage(
        &self,
        parent: Option<&MemoryPending>,
        block: BlockNumHash,
        diff: &HashedPostState,
    ) -> Result<(MemoryPending, B256), StateStoreError> {
        let (parent_number, base) = match parent {
            Some(parent) => (Some(parent.0.block.number), parent.0.state.clone()),
            None => {
                let inner = self.inner.read();
                (inner.persisted.last().map(|(b, _, _)| b.number), inner.tip_state())
            }
        };
        if let Some(parent_number) = parent_number {
            ensure_follows(parent_number, block.number)?;
        }
        let mut state = (*base).clone();
        state.apply(diff);
        let root = state.root();
        Ok((MemoryPending(Arc::new(PendingBlock { block, root, state: Arc::new(state) })), root))
    }

    fn persist(&self, chain: Vec<MemoryPending>) -> Result<(), StateStoreError> {
        let mut inner = self.inner.write();
        for pending in chain {
            if let Some((tip, _, _)) = inner.persisted.last() {
                ensure_follows(tip.number, pending.0.block.number)?;
            }
            inner.persisted.push((pending.0.block, pending.0.root, pending.0.state.clone()));
        }
        Ok(())
    }

    fn tip(&self) -> Result<Option<(BlockNumHash, B256)>, StateStoreError> {
        Ok(self.inner.read().persisted.last().map(|(block, root, _)| (*block, *root)))
    }

    fn unwind(&self, to: BlockNumber) -> Result<(), StateStoreError> {
        let mut inner = self.inner.write();
        let keep = inner
            .persisted
            .iter()
            .position(|(block, _, _)| block.number == to)
            .ok_or(StateStoreError::UnwindUnavailable(to))?;
        inner.persisted.truncate(keep + 1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_trie_common::HashedStorage;

    fn block(number: u64) -> BlockNumHash {
        BlockNumHash::new(number, B256::with_last_byte(number as u8))
    }

    fn account(nonce: u64) -> Account {
        Account { nonce, balance: U256::from(nonce * 10), bytecode_hash: None }
    }

    fn diff(accounts: &[(u8, Option<u64>)], slots: &[(u8, u8, u64)]) -> HashedPostState {
        let mut out = HashedPostState::default();
        for (addr, nonce) in accounts {
            out.accounts.insert(B256::repeat_byte(*addr), nonce.map(account));
        }
        for (addr, slot, value) in slots {
            out.storages
                .entry(B256::repeat_byte(*addr))
                .or_insert_with(HashedStorage::default)
                .storage
                .insert(B256::repeat_byte(*slot), U256::from(*value));
        }
        out
    }

    fn genesis() -> MemoryStateStore {
        MemoryStateStore::with_genesis(block(0), &diff(&[(1, Some(1))], &[(1, 1, 100)]))
    }

    #[test]
    fn reads_go_through_pending_chain() {
        let store = genesis();
        let (b1, _) = store.stage(None, block(1), &diff(&[(2, Some(5))], &[(1, 1, 101)])).unwrap();
        let (b2, _) = store.stage(Some(&b1), block(2), &diff(&[], &[(1, 1, 102)])).unwrap();

        let a = B256::repeat_byte(1);
        let s = B256::repeat_byte(1);
        assert_eq!(store.reader(None).unwrap().storage(a, s).unwrap(), U256::from(100));
        assert_eq!(store.reader(Some(&b1)).unwrap().storage(a, s).unwrap(), U256::from(101));
        assert_eq!(store.reader(Some(&b2)).unwrap().storage(a, s).unwrap(), U256::from(102));
        assert_eq!(
            store.reader(Some(&b2)).unwrap().account(B256::repeat_byte(2)).unwrap(),
            Some(account(5))
        );
        assert_eq!(store.reader(None).unwrap().account(B256::repeat_byte(2)).unwrap(), None);
    }

    #[test]
    fn siblings_are_independent() {
        let store = genesis();
        let (b1, _) = store.stage(None, block(1), &diff(&[], &[])).unwrap();
        let (left, left_root) =
            store.stage(Some(&b1), block(2), &diff(&[(3, Some(1))], &[])).unwrap();
        let (right, right_root) =
            store.stage(Some(&b1), block(2), &diff(&[(4, Some(1))], &[])).unwrap();

        assert_ne!(left_root, right_root);
        assert!(store
            .reader(Some(&left))
            .unwrap()
            .account(B256::repeat_byte(4))
            .unwrap()
            .is_none());
        assert!(store
            .reader(Some(&right))
            .unwrap()
            .account(B256::repeat_byte(3))
            .unwrap()
            .is_none());
    }

    #[test]
    fn deletes_remove_entries() {
        let store = genesis();
        let (_, deleted_root) =
            store.stage(None, block(1), &diff(&[(1, None)], &[(1, 1, 0)])).unwrap();
        let empty = MemoryStateStore::with_genesis(block(0), &HashedPostState::default());
        assert_eq!(deleted_root, empty.tip().unwrap().unwrap().1);
    }

    #[test]
    fn root_ignores_insertion_order_but_not_values() {
        let store = genesis();
        let forward = diff(&[(2, Some(1)), (3, Some(2))], &[(2, 1, 5), (2, 2, 6)]);
        let backward = diff(&[(3, Some(2)), (2, Some(1))], &[(2, 2, 6), (2, 1, 5)]);
        let changed = diff(&[(2, Some(1)), (3, Some(2))], &[(2, 1, 5), (2, 2, 7)]);
        let root = |d: &HashedPostState| store.stage(None, block(1), d).unwrap().1;
        assert_eq!(root(&forward), root(&backward));
        assert_ne!(root(&forward), root(&changed));
    }

    #[test]
    fn persist_requires_contiguous_blocks() {
        let store = genesis();
        let (b1, b1_root) = store.stage(None, block(1), &diff(&[(2, Some(1))], &[])).unwrap();
        let (b2, b2_root) = store.stage(Some(&b1), block(2), &diff(&[(2, Some(2))], &[])).unwrap();

        assert!(matches!(
            store.persist(vec![b2.clone()]),
            Err(StateStoreError::NotContiguous { expected: 1, got: 2 })
        ));
        assert!(store.stage(Some(&b1), block(3), &HashedPostState::default()).is_err());

        store.persist(vec![b1, b2]).unwrap();
        assert_eq!(store.tip().unwrap(), Some((block(2), b2_root)));
        assert_eq!(
            store.reader(None).unwrap().account(B256::repeat_byte(2)).unwrap(),
            Some(account(2))
        );

        store.unwind(1).unwrap();
        assert_eq!(store.tip().unwrap(), Some((block(1), b1_root)));
        assert_eq!(
            store.reader(None).unwrap().account(B256::repeat_byte(2)).unwrap(),
            Some(account(1))
        );
        assert!(matches!(store.unwind(5), Err(StateStoreError::UnwindUnavailable(5))));
    }
}
