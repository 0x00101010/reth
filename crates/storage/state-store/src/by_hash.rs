//! Object-safe, block-hash-keyed view of a [`StateStore`], and the process-wide instance the node
//! installs.
//!
//! reth's engine and persistence identify blocks by hash and only know a block's parent hash, so
//! [`ByHash`] keeps each staged block's [`StateStore::Pending`] in a map and resolves parents and
//! persisted chains from it. A global keeps the integration out of reth's generic types: when
//! [`global`] is set, the node keeps current state only in this store.

use crate::{StateReader, StateStore, StateStoreError};
use alloy_eips::BlockNumHash;
use alloy_primitives::{map::B256Map, B256, U256};
use parking_lot::Mutex;
use reth_primitives_traits::Account;
use reth_trie_common::HashedPostState;
use std::sync::{Arc, OnceLock};

/// Current state plus commitment, keyed by block hash. See the module docs.
pub trait BlockStateStore: Send + Sync + core::fmt::Debug {
    /// Account at the persisted tip.
    fn account(&self, hashed_address: B256) -> Result<Option<Account>, StateStoreError>;

    /// Storage value at the persisted tip; zero if absent.
    fn storage(&self, hashed_address: B256, hashed_slot: B256) -> Result<U256, StateStoreError>;

    /// Stages `block` on its parent (the persisted tip or a staged block) and returns its state
    /// root. Staging an already staged block returns its root.
    fn stage_block(
        &self,
        block: BlockNumHash,
        parent_hash: B256,
        diff: &HashedPostState,
    ) -> Result<B256, StateStoreError>;

    /// Persists every staged block from the persisted tip through `block`.
    fn persist_through(&self, block: BlockNumHash) -> Result<(), StateStoreError>;

    /// The persisted tip and its state root.
    fn tip(&self) -> Result<Option<(BlockNumHash, B256)>, StateStoreError>;
}

static GLOBAL: OnceLock<Arc<dyn BlockStateStore>> = OnceLock::new();

/// Installs the process-wide store. Returns the store back if one is already installed.
pub fn set_global(store: Arc<dyn BlockStateStore>) -> Result<(), Arc<dyn BlockStateStore>> {
    GLOBAL.set(store)
}

/// The process-wide store, if the node runs with one.
pub fn global() -> Option<&'static Arc<dyn BlockStateStore>> {
    GLOBAL.get()
}

#[derive(Debug)]
struct Staged<P> {
    pending: P,
    parent_hash: B256,
    number: u64,
    root: B256,
}

/// [`BlockStateStore`] over any [`StateStore`].
#[derive(Debug)]
pub struct ByHash<S: StateStore> {
    store: S,
    staged: Mutex<B256Map<Staged<S::Pending>>>,
}

impl<S: StateStore> ByHash<S> {
    /// Wraps `store`.
    pub fn new(store: S) -> Self {
        Self { store, staged: Mutex::default() }
    }
}

impl<S> BlockStateStore for ByHash<S>
where
    S: StateStore + core::fmt::Debug,
    S::Pending: core::fmt::Debug,
{
    fn account(&self, hashed_address: B256) -> Result<Option<Account>, StateStoreError> {
        self.store.reader(None)?.account(hashed_address)
    }

    fn storage(&self, hashed_address: B256, hashed_slot: B256) -> Result<U256, StateStoreError> {
        self.store.reader(None)?.storage(hashed_address, hashed_slot)
    }

    fn stage_block(
        &self,
        block: BlockNumHash,
        parent_hash: B256,
        diff: &HashedPostState,
    ) -> Result<B256, StateStoreError> {
        let parent = {
            let staged = self.staged.lock();
            if let Some(existing) = staged.get(&block.hash) {
                return Ok(existing.root)
            }
            match staged.get(&parent_hash) {
                Some(parent) => Some(parent.pending.clone()),
                None => match self.store.tip()? {
                    Some((tip, _)) if tip.hash == parent_hash => None,
                    _ => return Err(StateStoreError::UnknownParent(parent_hash)),
                },
            }
        };
        let (pending, root) = self.store.stage(parent.as_ref(), block, diff)?;
        self.staged
            .lock()
            .insert(block.hash, Staged { pending, parent_hash, number: block.number, root });
        Ok(root)
    }

    fn persist_through(&self, block: BlockNumHash) -> Result<(), StateStoreError> {
        let Some((tip, _)) = self.store.tip()? else {
            return Err(StateStoreError::Backend("persist into an empty store".into()))
        };
        if tip.number >= block.number {
            return Ok(())
        }
        let chain = {
            let staged = self.staged.lock();
            let mut chain = Vec::new();
            let mut hash = block.hash;
            while hash != tip.hash {
                let entry = staged.get(&hash).ok_or(StateStoreError::UnknownParent(hash))?;
                chain.push(entry.pending.clone());
                hash = entry.parent_hash;
            }
            chain.reverse();
            chain
        };
        self.store.persist(chain)?;
        self.staged.lock().retain(|_, entry| entry.number > block.number);
        Ok(())
    }

    fn tip(&self) -> Result<Option<(BlockNumHash, B256)>, StateStoreError> {
        self.store.tip()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStateStore;

    fn block(number: u64, fork: u8) -> BlockNumHash {
        let mut hash = [fork; 32];
        hash[31] = number as u8;
        BlockNumHash::new(number, B256::new(hash))
    }

    fn set(addr: u8, nonce: u64) -> HashedPostState {
        let mut diff = HashedPostState::default();
        diff.accounts
            .insert(B256::repeat_byte(addr), Some(Account { nonce, ..Default::default() }));
        diff
    }

    #[test]
    fn stages_forks_and_persists_by_hash() {
        let genesis = block(0, 0);
        let store =
            ByHash::new(MemoryStateStore::with_genesis(genesis, &HashedPostState::default()));
        let b1 = block(1, 0);
        let b2 = block(2, 0);
        let b2_fork = block(2, 1);
        let root1 = store.stage_block(b1, genesis.hash, &set(1, 1)).unwrap();
        assert_eq!(store.stage_block(b1, genesis.hash, &set(9, 9)).unwrap(), root1);
        store.stage_block(b2, b1.hash, &set(1, 2)).unwrap();
        store.stage_block(b2_fork, b1.hash, &set(1, 3)).unwrap();
        assert!(matches!(
            store.stage_block(block(3, 0), B256::repeat_byte(0xee), &set(1, 4)),
            Err(StateStoreError::UnknownParent(_))
        ));

        assert_eq!(store.account(B256::repeat_byte(1)).unwrap(), None);
        store.persist_through(b2_fork).unwrap();
        assert_eq!(store.tip().unwrap().unwrap().0, b2_fork);
        assert_eq!(store.account(B256::repeat_byte(1)).unwrap().unwrap().nonce, 3);
        // Persisting an older block is a no-op; staged blocks at or below the tip are gone.
        store.persist_through(b1).unwrap();
        assert!(store.staged.lock().is_empty());
        // Staging continues from the new tip.
        store.stage_block(block(3, 1), b2_fork.hash, &set(1, 5)).unwrap();
    }
}
