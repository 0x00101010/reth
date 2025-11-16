use crate::{FlashBlockCompleteSequence, FlashBlockCompleteSequenceRx};
use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadStatusEnum;
use op_alloy_rpc_types_engine::OpExecutionData;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_optimism_payload_builder::OpPayloadTypes;
use reth_payload_primitives::{EngineApiMessageVersion, ExecutionPayload, PayloadTypes};
use tracing::*;

/// Consensus client that sends FCUs and new payloads using blocks from a [`FlashBlockService`].
///
/// This client receives completed flashblock sequences and:
/// - Attempts to submit `engine_newPayload` if `state_root` is available (non-zero)
/// - Always sends `engine_forkChoiceUpdated` to drive chain forward
///
/// [`FlashBlockService`]: crate::FlashBlockService
#[derive(Debug)]
pub struct FlashBlockConsensusClient<P = OpPayloadTypes>
where
    P: PayloadTypes,
{
    /// Handle to execution client.
    engine_handle: ConsensusEngineHandle<P>,
    /// Receiver for completed flashblock sequences from `FlashBlockService`.
    sequence_receiver: FlashBlockCompleteSequenceRx,
}

impl<P> FlashBlockConsensusClient<P>
where
    P: PayloadTypes,
    P::ExecutionData: for<'a> TryFrom<&'a FlashBlockCompleteSequence, Error: std::fmt::Display>,
{
    /// Create a new `FlashBlockConsensusClient` with the given Op engine and sequence receiver.
    pub const fn new(
        engine_handle: ConsensusEngineHandle<P>,
        sequence_receiver: FlashBlockCompleteSequenceRx,
    ) -> eyre::Result<Self> {
        Ok(Self { engine_handle, sequence_receiver })
    }

    /// Attempts to submit a new payload to the engine.
    ///
    /// The `TryFrom` conversion will fail if `execution_outcome.state_root` is `B256::ZERO`,
    /// in which case this returns the `parent_hash` instead.
    ///
    /// Returns the block hash to use for FCU (either the new block or parent).
    async fn submit_new_payload(&self, sequence: &FlashBlockCompleteSequence) -> B256 {
        let payload = match P::ExecutionData::try_from(sequence) {
            Ok(payload) => payload,
            Err(err) => {
                warn!(target: "flashblocks", %err);
                return sequence.payload_base().parent_hash;
            }
        };

        let block_number = payload.block_number();
        let block_hash = payload.block_hash();
        match self.engine_handle.new_payload(payload).await {
            Ok(result) => {
                debug!(
                    target: "flashblocks",
                    flashblock_count = sequence.count(),
                    block_number,
                    %block_hash,
                    ?result,
                    "Submitted engine_newPayload",
                );

                if let PayloadStatusEnum::Invalid { validation_error } = result.status {
                    debug!(
                        target: "flashblocks",
                        block_number,
                        %block_hash,
                        %validation_error,
                        "Payload validation error",
                    );
                };
            }
            Err(err) => {
                error!(
                    target: "flashblocks",
                    %err,
                    block_number,
                    "Failed to submit new payload",
                );
            }
        }

        block_hash
    }

    /// Submit a forkchoice update to the engine.
    async fn submit_forkchoice_update(
        &self,
        head_block_hash: B256,
        sequence: &FlashBlockCompleteSequence,
    ) {
        let block_number = sequence.block_number();
        let safe_hash = sequence.payload_base().parent_hash;
        let finalized_hash = sequence.payload_base().parent_hash;
        let fcu_state = alloy_rpc_types_engine::ForkchoiceState {
            head_block_hash,
            safe_block_hash: safe_hash,
            finalized_block_hash: finalized_hash,
        };

        match self
            .engine_handle
            .fork_choice_updated(fcu_state, None, EngineApiMessageVersion::V5)
            .await
        {
            Ok(result) => {
                debug!(
                    target: "flashblocks",
                    flashblock_count = sequence.count(),
                    block_number,
                    %head_block_hash,
                    %safe_hash,
                    %finalized_hash,
                    ?result,
                    "Submitted engine_forkChoiceUpdated",
                )
            }
            Err(err) => {
                error!(
                    target: "flashblocks",
                    %err,
                    block_number,
                    %head_block_hash,
                    %safe_hash,
                    %finalized_hash,
                    "Failed to submit fork choice update",
                );
            }
        }
    }

    /// Runs the consensus client loop.
    ///
    /// Continuously receives completed flashblock sequences and submits them to the execution
    /// engine:
    /// 1. Attempts `engine_newPayload` (only if `state_root` is available)
    /// 2. Always sends `engine_forkChoiceUpdated` to drive chain forward
    pub async fn run(mut self) {
        loop {
            let Ok(sequence) = self.sequence_receiver.recv().await else {
                continue;
            };

            // Returns block_hash for FCU:
            // - If state_root is available: submits newPayload and returns the new block's hash
            // - If state_root is zero: skips newPayload and returns parent_hash (no progress yet)
            let block_hash = self.submit_new_payload(&sequence).await;

            self.submit_forkchoice_update(block_hash, &sequence).await;
        }
    }
}

impl TryFrom<&FlashBlockCompleteSequence> for OpExecutionData {
    type Error = &'static str;

    fn try_from(sequence: &FlashBlockCompleteSequence) -> Result<Self, Self::Error> {
        let mut data = Self::from_flashblocks_unchecked(sequence);

        // If execution outcome is available, use the computed state_root and block_hash.
        // FlashBlockService computes these when building sequences on top of the local tip.
        if let Some(execution_outcome) = sequence.execution_outcome() {
            let payload = data.payload.as_v1_mut();
            payload.state_root = execution_outcome.state_root;
            payload.block_hash = execution_outcome.block_hash;
        }

        // Only proceed if we have a valid state_root (non-zero).
        if data.payload.as_v1_mut().state_root == B256::ZERO {
            return Err("No state_root available for payload");
        }

        Ok(data)
    }
}
