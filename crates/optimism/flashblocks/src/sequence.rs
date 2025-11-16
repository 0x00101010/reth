use crate::{FlashBlock, FlashBlockCompleteSequenceRx};
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_engine::PayloadId;
use core::mem;
use eyre::{bail, OptionExt};
use op_alloy_rpc_types_engine::OpFlashblockPayloadBase;
use reth_revm::cached::CachedReads;
use std::{collections::BTreeMap, ops::Deref};
use tokio::sync::broadcast;
use tracing::{debug, trace, warn};

/// The size of the broadcast channel for completed flashblock sequences.
const FLASHBLOCK_SEQUENCE_CHANNEL_SIZE: usize = 128;

/// Outcome from executing a flashblock sequence.
#[derive(Debug, Clone, Copy)]
pub struct SequenceExecutionOutcome {
    /// The block hash of the executed pending block
    pub block_hash: B256,
    /// Properly computed state root
    pub state_root: B256,
}

/// An ordered B-tree keeping the track of a sequence of [`FlashBlock`]s by their indices.
#[derive(Debug)]
pub struct FlashBlockPendingSequence {
    /// tracks the individual flashblocks in order
    inner: BTreeMap<u64, FlashBlock>,
    /// Broadcasts flashblocks to subscribers.
    block_broadcaster: broadcast::Sender<FlashBlockCompleteSequence>,
    /// Optional execution outcome from building the current sequence.
    execution_outcome: Option<SequenceExecutionOutcome>,
    /// Cached state reads for the current block.
    /// Current `PendingFlashBlock` is built out of a sequence of `FlashBlocks`, and executed again
    /// when fb received on top of the same block. Avoid redundant I/O across multiple
    /// executions within the same block.
    cached_reads: Option<CachedReads>,
}

impl FlashBlockPendingSequence {
    /// Create a new pending sequence.
    pub fn new() -> Self {
        // Note: if the channel is full, send will not block but rather overwrite the oldest
        // messages. Order is preserved.
        let (tx, _) = broadcast::channel(FLASHBLOCK_SEQUENCE_CHANNEL_SIZE);
        Self {
            inner: BTreeMap::new(),
            block_broadcaster: tx,
            execution_outcome: None,
            cached_reads: None,
        }
    }

    /// Returns the sender half of the [`FlashBlockCompleteSequence`] channel.
    pub const fn block_sequence_broadcaster(
        &self,
    ) -> &broadcast::Sender<FlashBlockCompleteSequence> {
        &self.block_broadcaster
    }

    /// Gets a subscriber to the flashblock sequences produced.
    pub fn subscribe_block_sequence(&self) -> FlashBlockCompleteSequenceRx {
        self.block_broadcaster.subscribe()
    }

    // Clears the state and broadcasts the blocks produced to subscribers.
    fn clear_and_broadcast_blocks(&mut self) {
        if self.inner.is_empty() {
            return;
        }

        let flashblocks = mem::take(&mut self.inner);
        let execution_outcome = mem::take(&mut self.execution_outcome);

        // If there are any subscribers, send the flashblocks to them.
        if self.block_broadcaster.receiver_count() > 0 {
            let flashblocks = match FlashBlockCompleteSequence::new(
                flashblocks.into_iter().map(|block| block.1).collect(),
                execution_outcome,
            ) {
                Ok(flashblocks) => flashblocks,
                Err(err) => {
                    debug!(target: "flashblocks", error = ?err, "Failed to create full flashblock complete sequence");
                    return;
                }
            };

            // Note: this should only ever fail if there are no receivers. This can happen if
            // there is a race condition between the clause right above and this
            // one. We can simply warn the user and continue.
            if let Err(err) = self.block_broadcaster.send(flashblocks) {
                warn!(target: "flashblocks", error = ?err, "Failed to send flashblocks to subscribers");
            }
        }
    }

    /// Inserts a new block into the sequence.
    ///
    /// A [`FlashBlock`] with index 0 resets the set.
    pub fn insert(&mut self, flashblock: FlashBlock) {
        if flashblock.index == 0 {
            trace!(target: "flashblocks", number=%flashblock.block_number(), "Tracking new flashblock sequence");

            // Flash block at index zero resets the whole state.
            self.clear_and_broadcast_blocks();

            self.inner.insert(flashblock.index, flashblock);
            return;
        }

        // only insert if we previously received the same block and payload, assume we received
        // index 0
        let same_block = self.block_number() == Some(flashblock.block_number());
        let same_payload = self.payload_id() == Some(flashblock.payload_id);

        if same_block && same_payload {
            trace!(target: "flashblocks", number=%flashblock.block_number(), index = %flashblock.index, block_count = self.inner.len()  ,"Received followup flashblock");
            self.inner.insert(flashblock.index, flashblock);
        } else {
            trace!(target: "flashblocks", number=%flashblock.block_number(), index = %flashblock.index, current=?self.block_number()  ,"Ignoring untracked flashblock following");
        }
    }

    /// Set execution outcome from building the flashblock sequence
    pub const fn set_execution_outcome(
        &mut self,
        execution_outcome: Option<SequenceExecutionOutcome>,
    ) {
        self.execution_outcome = execution_outcome;
    }

    /// Set cached reads for this sequence
    pub fn set_cached_reads(&mut self, cached_reads: CachedReads) {
        self.cached_reads = Some(cached_reads);
    }

    /// Returns cached reads for this sequence
    pub const fn cached_reads(&self) -> &Option<CachedReads> {
        &self.cached_reads
    }

    /// Returns the first block number
    pub fn block_number(&self) -> Option<u64> {
        Some(self.inner.values().next()?.block_number())
    }

    /// Returns the payload base of the first tracked flashblock.
    pub fn payload_base(&self) -> Option<OpFlashblockPayloadBase> {
        self.inner.values().next()?.base.clone()
    }

    /// Returns the number of tracked flashblocks.
    pub fn count(&self) -> usize {
        self.inner.len()
    }

    /// Returns the reference to the last flashblock.
    pub fn last_flashblock(&self) -> Option<&FlashBlock> {
        self.inner.last_key_value().map(|(_, b)| b)
    }

    /// Returns the current/latest flashblock index in the sequence
    pub fn index(&self) -> Option<u64> {
        Some(self.inner.values().last()?.index)
    }
    /// Returns the payload id of the first tracked flashblock in the current sequence.
    pub fn payload_id(&self) -> Option<PayloadId> {
        Some(self.inner.values().next()?.payload_id)
    }

    /// Finalizes the current pending sequence and returns it as a complete sequence.
    ///
    /// Clears the internal state and returns an error if the sequence is empty or validation fails.
    pub fn finalize(&mut self) -> eyre::Result<FlashBlockCompleteSequence> {
        if self.inner.is_empty() {
            bail!("Cannot finalize empty flashblock sequence");
        }

        let flashblocks = mem::take(&mut self.inner);
        let execution_outcome = mem::take(&mut self.execution_outcome);

        FlashBlockCompleteSequence::new(flashblocks.into_values().collect(), execution_outcome)
    }

    /// Returns an iterator over all flashblocks in the sequence.
    pub fn flashblocks(&self) -> impl Iterator<Item = &FlashBlock> {
        self.inner.values()
    }
}

impl Default for FlashBlockPendingSequence {
    fn default() -> Self {
        Self::new()
    }
}

/// A complete sequence of flashblocks, often corresponding to a full block.
///
/// Ensures invariants of a complete flashblocks sequence.
/// If this entire sequence of flashblocks was executed on top of latest block, this also includes
/// the execution outcome with block hash and state root.
#[derive(Debug, Clone)]
pub struct FlashBlockCompleteSequence {
    inner: Vec<FlashBlock>,
    /// Optional execution outcome from building the flashblock sequence
    execution_outcome: Option<SequenceExecutionOutcome>,
}

impl FlashBlockCompleteSequence {
    /// Create a complete sequence from a vector of flashblocks.
    /// Ensure that:
    /// * vector is not empty
    /// * first flashblock have the base payload
    /// * sequence of flashblocks is sound (successive index from 0, same payload id, ...)
    pub fn new(
        blocks: Vec<FlashBlock>,
        execution_outcome: Option<SequenceExecutionOutcome>,
    ) -> eyre::Result<Self> {
        let first_block = blocks.first().ok_or_eyre("No flashblocks in sequence")?;

        // Ensure that first flashblock have base
        first_block.base.as_ref().ok_or_eyre("Flashblock at index 0 has no base")?;

        // Ensure that index are successive from 0, have same block number and payload id
        if !blocks.iter().enumerate().all(|(idx, block)| {
            idx == block.index as usize &&
                block.payload_id == first_block.payload_id &&
                block.block_number() == first_block.block_number()
        }) {
            bail!("Flashblock inconsistencies detected in sequence");
        }

        Ok(Self { inner: blocks, execution_outcome })
    }

    /// Returns the block number
    pub fn block_number(&self) -> u64 {
        self.inner.first().unwrap().block_number()
    }

    /// Returns the payload base of the first flashblock.
    pub fn payload_base(&self) -> &OpFlashblockPayloadBase {
        self.inner.first().unwrap().base.as_ref().unwrap()
    }

    /// Returns the number of flashblocks in the sequence.
    pub const fn count(&self) -> usize {
        self.inner.len()
    }

    /// Returns the last flashblock in the sequence.
    pub fn last(&self) -> &FlashBlock {
        self.inner.last().unwrap()
    }

    /// Returns the execution outcome of the sequence.
    pub const fn execution_outcome(&self) -> Option<SequenceExecutionOutcome> {
        self.execution_outcome
    }

    /// Updates execution outcome of the sequence.
    pub const fn set_execution_outcome(
        &mut self,
        execution_outcome: Option<SequenceExecutionOutcome>,
    ) {
        self.execution_outcome = execution_outcome;
    }

    /// Returns all transactions from all flashblocks in the sequence
    pub fn all_transactions(&self) -> Vec<Bytes> {
        self.inner.iter().flat_map(|fb| fb.diff.transactions.iter().cloned()).collect()
    }
}

impl Deref for FlashBlockCompleteSequence {
    type Target = Vec<FlashBlock>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl TryFrom<FlashBlockPendingSequence> for FlashBlockCompleteSequence {
    type Error = eyre::Error;
    fn try_from(sequence: FlashBlockPendingSequence) -> Result<Self, Self::Error> {
        Self::new(sequence.inner.into_values().collect(), sequence.execution_outcome)
    }
}

#[cfg(test)]
mod tests {
    // TODO: Add tests for:
    // - FlashBlockPendingSequence insert() behavior
    // - FlashBlockPendingSequence finalize() correctness
    // - FlashBlockCompleteSequence invariants
    // Note: Transaction recovery and broadcasting tests moved to cache.rs SequenceManager tests
}
