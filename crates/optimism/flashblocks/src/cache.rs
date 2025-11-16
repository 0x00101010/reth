//! Sequence cache management for flashblocks.
//!
//! The `SequenceManager` maintains a ring buffer of recently completed flashblock sequences
//! and intelligently selects which sequence to build based on the local chain tip.

use crate::{
    sequence::{FlashBlockPendingSequence, SequenceExecutionOutcome},
    worker::BuildArgs,
    FlashBlock, FlashBlockCompleteSequence, PendingFlashBlock,
};
use alloy_eips::eip2718::WithEncoded;
use alloy_primitives::B256;
use reth_primitives_traits::{NodePrimitives, Recovered, SignedTransaction};
use reth_revm::cached::CachedReads;
use ringbuffer::{AllocRingBuffer, RingBuffer};
use tokio::sync::broadcast;
use tracing::{debug, trace};

/// Maximum number of cached sequences in the ring buffer.
const CACHE_SIZE: usize = 3;
/// 200 ms flashblock time.
pub(crate) const FLASHBLOCK_BLOCK_TIME: u64 = 200;

/// Manages flashblock sequences with caching support.
///
/// This struct handles:
/// - Tracking the current pending sequence
/// - Caching completed sequences in a fixed-size ring buffer
/// - Finding the best sequence to build based on local chain tip
/// - Broadcasting completed sequences to subscribers
#[derive(Debug)]
pub(crate) struct SequenceManager<T> {
    /// Current pending sequence being built up from incoming flashblocks
    pending: FlashBlockPendingSequence,
    /// Ring buffer of recently completed sequences (FIFO, size 3)
    completed_cache: AllocRingBuffer<CachedSequence>,
    /// Broadcast channel for completed sequences
    block_broadcaster: broadcast::Sender<FlashBlockCompleteSequence>,
    /// Whether to compute state roots when building blocks
    compute_state_root: bool,
    /// Phantom data for transaction type
    _phantom: std::marker::PhantomData<T>,
}

/// A cached completed flashblock sequence with associated metadata.
#[derive(Debug, Clone)]
pub(crate) struct CachedSequence {
    /// Block number of this sequence
    pub block_number: u64,
    /// Parent hash that this sequence builds on top of
    pub parent_hash: B256,
    /// The complete flashblock sequence
    pub sequence: FlashBlockCompleteSequence,
    /// Cached reads from when this sequence was executed
    pub cached_reads: Option<CachedReads>,
}

impl<T: SignedTransaction> SequenceManager<T> {
    /// Creates a new sequence manager.
    pub(crate) fn new(compute_state_root: bool) -> Self {
        let (block_broadcaster, _) = broadcast::channel(128);
        Self {
            pending: FlashBlockPendingSequence::new(),
            completed_cache: AllocRingBuffer::new(CACHE_SIZE),
            block_broadcaster,
            compute_state_root,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Returns the sender half of the flashblock sequence broadcast channel.
    pub(crate) const fn block_sequence_broadcaster(
        &self,
    ) -> &broadcast::Sender<FlashBlockCompleteSequence> {
        &self.block_broadcaster
    }

    /// Gets a subscriber to the flashblock sequences produced.
    pub(crate) fn subscribe_block_sequence(&self) -> crate::FlashBlockCompleteSequenceRx {
        self.block_broadcaster.subscribe()
    }

    /// Inserts a new flashblock into the pending sequence.
    ///
    /// When a flashblock with index 0 arrives (indicating a new block), the current
    /// pending sequence is finalized, cached, and broadcast to subscribers immediately
    /// (even if state_root hasn't been computed yet).
    pub(crate) fn insert_flashblock(&mut self, flashblock: FlashBlock) -> eyre::Result<()> {
        // If this starts a new block, finalize and cache the previous sequence BEFORE inserting
        if flashblock.index == 0 {
            let completed = self.pending.finalize()?;
            let block_number = completed.block_number();
            let parent_hash = completed.payload_base().parent_hash;

            trace!(
                target: "flashblocks",
                block_number,
                %parent_hash,
                cache_size = self.completed_cache.len(),
                "Caching completed flashblock sequence"
            );

            // Add to cache (FIFO - oldest gets pushed out if full)
            let cached = CachedSequence {
                block_number,
                parent_hash,
                sequence: completed.clone(),
                cached_reads: None, // Will be populated after execution
            };

            self.completed_cache.push(cached);

            // Broadcast immediately to subscribers (even without state_root)
            // ConsensusClient will check execution_outcome before sending newPayload
            if self.block_broadcaster.receiver_count() > 0 {
                let _ = self.block_broadcaster.send(completed);
            }
        }

        // Now insert the new flashblock
        self.pending.insert(flashblock);
        Ok(())
    }

    /// Returns the current pending sequence for inspection.
    pub(crate) const fn pending(&self) -> &FlashBlockPendingSequence {
        &self.pending
    }

    /// Finds the next sequence to build and returns ready-to-use BuildArgs.
    ///
    /// Priority order:
    /// 1. Current pending sequence (if parent matches local tip)
    /// 2. Cached sequence with exact parent match
    ///
    /// Returns None if nothing is buildable right now.
    pub(crate) fn next_buildable_args(
        &mut self,
        local_tip_hash: B256,
        local_tip_number: u64,
    ) -> Option<BuildArgs<Vec<WithEncoded<Recovered<T>>>>> {
        // Try to find a buildable sequence: (base, last_fb, flashblocks, cached_state, source_name)
        let (base, last_flashblock, flashblocks, cached_state, source_name) =
            // Priority 1: Try current pending sequence
            if let Some(base) = self.pending.payload_base().filter(|b| b.parent_hash == local_tip_hash) {
                let last_fb = self.pending.last_flashblock()?;
                let flashblocks = self.pending.flashblocks().collect::<Vec<_>>();
                let cached_state = self.pending.cached_reads().as_ref().map(|r| (base.parent_hash, r.clone()));
                (base, last_fb, flashblocks, cached_state, "pending")
            }
            // Priority 2: Try cached sequence with exact parent match
            else if let Some(cached) = self.completed_cache.iter().find(|c| c.parent_hash == local_tip_hash) {
                let base = cached.sequence.payload_base().clone();
                let last_fb = cached.sequence.last();
                let flashblocks = cached.sequence.iter().collect::<Vec<_>>();
                let cached_state = cached.cached_reads.as_ref().map(|r| (cached.parent_hash, r.clone()));
                (base, last_fb, flashblocks, cached_state, "cached")
            } else {
                return None;
            };

        debug!(
            target: "flashblocks",
            block_number = base.block_number,
            source = source_name,
            "Building from flashblock sequence"
        );

        // Auto-detect when to compute state root: only if the builder didn't provide it (sent
        // B256::ZERO) and we're near the expected final flashblock index.
        //
        // Background: Each block period receives multiple flashblocks at regular intervals.
        // The sequencer sends an initial "base" flashblock at index 0 when a new block starts,
        // then subsequent flashblocks are produced every FLASHBLOCK_BLOCK_TIME intervals (200ms).
        //
        // Examples with different block times:
        // - Base (2s blocks):    expect 2000ms / 200ms = 10 intervals → Flashblocks: index 0 (base)
        //   + indices 1-10 = potentially 11 total
        //
        // - Unichain (1s blocks): expect 1000ms / 200ms = 5 intervals → Flashblocks: index 0 (base)
        //   + indices 1-5 = potentially 6 total
        //
        // Why compute at N-1 instead of N:
        // 1. Timing variance in flashblock producing time may mean only N flashblocks were produced
        //    instead of N+1 (missing the final one). Computing at N-1 ensures we get the state root
        //    for most common cases.
        //
        // 2. The +1 case (index 0 base + N intervals): If all N+1 flashblocks do arrive, we'll
        //    still calculate state root for flashblock N, which sacrifices a little performance but
        //    still ensures correctness for common cases.
        //
        // Note: Pathological cases may result in fewer flashblocks than expected (e.g., builder
        // downtime, flashblock execution exceeding timing budget). When this occurs, we won't
        // compute the state root, causing FlashblockConsensusClient to lack precomputed state for
        // engine_newPayload. This is safe: we still have op-node as backstop to maintain
        // chain progression.
        let block_time_ms = (base.timestamp - local_tip_number) * 1000;
        let expected_final_flashblock = block_time_ms / FLASHBLOCK_BLOCK_TIME;
        let compute_state_root = self.compute_state_root
            && last_flashblock.diff.state_root.is_zero()
            && last_flashblock.index >= expected_final_flashblock.saturating_sub(1);

        let transactions = self.recover_transactions(flashblocks.into_iter())?;

        Some(BuildArgs {
            base,
            transactions,
            cached_state,
            last_flashblock_index: last_flashblock.index,
            last_flashblock_hash: last_flashblock.diff.block_hash,
            compute_state_root,
        })
    }

    /// Recovers transactions from flashblocks lazily.
    ///
    /// This is done only when we actually need to build a sequence, avoiding wasted computation.
    fn recover_transactions<'a>(
        &self,
        flashblocks: impl Iterator<Item = &'a FlashBlock>,
    ) -> Option<Vec<WithEncoded<Recovered<T>>>> {
        let mut transactions = Vec::new();

        for flashblock in flashblocks {
            for encoded in &flashblock.diff.transactions {
                let tx = T::decode_2718_exact(encoded.as_ref()).ok()?;
                let signer = tx.try_recover().ok()?;
                transactions.push(WithEncoded::new(encoded.clone(), tx.with_signer(signer)));
            }
        }

        Some(transactions)
    }

    /// Records the result of building a sequence.
    ///
    /// Updates both execution outcome and cached reads for whichever sequence was built
    /// (pending or cached).
    pub(crate) fn on_build_complete<N: NodePrimitives>(
        &mut self,
        parent_hash: B256,
        result: Option<(PendingFlashBlock<N>, CachedReads)>,
    ) {
        let Some((computed_block, cached_reads)) = result else {
            return;
        };

        // Extract execution outcome
        let execution_outcome = computed_block.computed_state_root().map(|state_root| {
            SequenceExecutionOutcome {
                block_hash: computed_block.block().hash(),
                state_root,
            }
        });

        if self.pending.payload_base().is_some_and(|base| base.parent_hash == parent_hash) {
            self.pending.set_execution_outcome(execution_outcome);
            self.pending.set_cached_reads(cached_reads);
            trace!(
                target: "flashblocks",
                block_number = self.pending.block_number(),
                has_computed_state_root = execution_outcome.is_some(),
                "Updated pending sequence with build results"
            );
            return;
        };

        // Building from cached - update cached reads
        if let Some(cached) = self
            .completed_cache
            .iter_mut()
            .find(|c| c.parent_hash == parent_hash)
        {
            cached.sequence.set_execution_outcome(execution_outcome);
            cached.cached_reads = Some(cached_reads);
            trace!(
                target: "flashblocks",
                block_number = cached.block_number,
                has_state_root = execution_outcome.is_some(),
                "Updated cached sequence with build results"
            );
        }
    }

    /// Updates cached reads for the new canonical tip.
    ///
    /// Pre-fills the cache with state from the new canonical block to avoid
    /// redundant I/O when building subsequent blocks.
    pub(crate) fn on_new_canonical_tip(&mut self, tip_hash: B256, cached: CachedReads) {
        // Store for potential use in next build
        if let Some(cached_seq) = self
            .completed_cache
            .iter_mut()
            .find(|c| c.parent_hash == tip_hash)
        {
            cached_seq.cached_reads = Some(cached);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: Add tests for:
    // - insert_flashblock with index 0 caching and broadcasting
    // - find_buildable_args priority order
    // - cache eviction (FIFO)
    // - on_build_complete updating cache vs pending
}