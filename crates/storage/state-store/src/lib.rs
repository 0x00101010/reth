//! Current state plus its commitment, at whole-block granularity.
//!
//! A [`StateStore`] owns the latest persisted state and computes the state commitment for each
//! block. Executed blocks that are not yet persisted live as [`StateStore::Pending`] values that
//! chain onto each other (and may fork), mirroring the engine's in-memory tree.
//!
//! Keys are hashed, matching reth's storage v2 layout where the hashed state tables are
//! canonical: an account is keyed by `keccak256(address)` and a slot by
//! `keccak256(keccak256(address) || keccak256(slot))` (see [`storage_key`]).

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_eips::BlockNumHash;
use alloy_primitives::{keccak256, BlockNumber, B256, KECCAK256_EMPTY as KECCAK_EMPTY, U256};
use reth_primitives_traits::Account;
use reth_trie_common::HashedPostState;

mod memory;
pub use memory::{MemoryPending, MemoryReader, MemoryStateStore};

/// Current state plus its commitment.
///
/// `diff` passed to [`StateStore::stage`] is one block's net effect per key: an account of
/// `None` deletes the account, a slot value of zero deletes the slot.
pub trait StateStore: Send + Sync {
    /// State of an executed block that is not persisted yet.
    type Pending: Clone + Send + Sync;
    /// Point-in-time reader.
    type Reader: StateReader;

    /// Reader at the persisted tip (`None`) or at a pending block.
    fn reader(&self, at: Option<&Self::Pending>) -> Result<Self::Reader, StateStoreError>;

    /// Stages `block` on top of `parent` (`None` = the persisted tip) and returns its state and
    /// state root.
    fn stage(
        &self,
        parent: Option<&Self::Pending>,
        block: BlockNumHash,
        diff: &HashedPostState,
    ) -> Result<(Self::Pending, B256), StateStoreError>;

    /// Durably applies a chain of pending blocks, oldest first, starting right after the
    /// persisted tip.
    fn persist(&self, chain: Vec<Self::Pending>) -> Result<(), StateStoreError>;

    /// The persisted tip and its state root, or `None` for an empty store.
    fn tip(&self) -> Result<Option<(BlockNumHash, B256)>, StateStoreError>;

    /// Drops persisted blocks above `to`.
    fn unwind(&self, to: BlockNumber) -> Result<(), StateStoreError>;
}

/// Reads accounts and storage by hashed key.
pub trait StateReader {
    /// Account at `hashed_address`.
    fn account(&self, hashed_address: B256) -> Result<Option<Account>, StateStoreError>;

    /// Storage value of `hashed_slot` under `hashed_address`; zero if absent.
    fn storage(&self, hashed_address: B256, hashed_slot: B256) -> Result<U256, StateStoreError>;
}

/// [`StateStore`] errors.
#[derive(Debug, thiserror::Error)]
pub enum StateStoreError {
    /// A block does not directly follow its parent or the persisted tip.
    #[error("block {got} does not follow block {expected}")]
    NotContiguous {
        /// Number the block should have had.
        expected: BlockNumber,
        /// Number it had.
        got: BlockNumber,
    },
    /// The store cannot unwind to the requested block.
    #[error("cannot unwind to block {0}")]
    UnwindUnavailable(BlockNumber),
    /// Backend failure.
    #[error(transparent)]
    Backend(Box<dyn core::error::Error + Send + Sync>),
}

/// Store key of a storage slot: `keccak256(hashed_address || hashed_slot)`.
pub fn storage_key(hashed_address: B256, hashed_slot: B256) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(hashed_address.as_slice());
    buf[32..].copy_from_slice(hashed_slot.as_slice());
    keccak256(buf)
}

/// Encoded account length: `nonce (8) || balance (32) || code_hash (32)`.
pub const ACCOUNT_LEN: usize = 72;

/// Encodes an account as `nonce (u64 BE) || balance (U256 BE) || code_hash`, where an account
/// without code has [`KECCAK_EMPTY`].
pub fn encode_account(account: &Account) -> [u8; ACCOUNT_LEN] {
    let mut out = [0u8; ACCOUNT_LEN];
    out[..8].copy_from_slice(&account.nonce.to_be_bytes());
    out[8..40].copy_from_slice(&account.balance.to_be_bytes::<32>());
    out[40..].copy_from_slice(account.bytecode_hash.unwrap_or(KECCAK_EMPTY).as_slice());
    out
}

/// Inverse of [`encode_account`].
pub fn decode_account(bytes: &[u8; ACCOUNT_LEN]) -> Account {
    let code_hash = B256::from_slice(&bytes[40..]);
    Account {
        nonce: u64::from_be_bytes(bytes[..8].try_into().expect("8 bytes")),
        balance: U256::from_be_slice(&bytes[8..40]),
        bytecode_hash: (code_hash != KECCAK_EMPTY).then_some(code_hash),
    }
}

/// Checks that `block` directly follows `parent`.
pub const fn ensure_follows(
    parent: BlockNumber,
    block: BlockNumber,
) -> Result<(), StateStoreError> {
    if block == parent + 1 {
        Ok(())
    } else {
        Err(StateStoreError::NotContiguous { expected: parent + 1, got: block })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_encoding_round_trips() {
        let with_code = Account {
            nonce: 7,
            balance: U256::from(1_000u64),
            bytecode_hash: Some(B256::repeat_byte(9)),
        };
        let without_code = Account { nonce: 0, balance: U256::MAX, bytecode_hash: None };
        for account in [with_code, without_code] {
            assert_eq!(decode_account(&encode_account(&account)), account);
        }
        assert_eq!(&encode_account(&without_code)[40..], KECCAK_EMPTY.as_slice());
    }

    #[test]
    fn storage_key_depends_on_both_halves() {
        let a = B256::repeat_byte(1);
        let s = B256::repeat_byte(2);
        assert_ne!(storage_key(a, s), storage_key(s, a));
        assert_ne!(storage_key(a, s), a);
    }
}
