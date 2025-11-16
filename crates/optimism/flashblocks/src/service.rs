use crate::{
    cache::SequenceManager, worker::FlashBlockBuilder, FlashBlock, FlashBlockCompleteSequence,
    FlashBlockCompleteSequenceRx, InProgressFlashBlockRx, PendingFlashBlock,
};
use alloy_primitives::B256;
use futures_util::{Stream, StreamExt};
use metrics::Histogram;
use op_alloy_rpc_types_engine::OpFlashblockPayloadBase;
use reth_chain_state::{CanonStateNotifications, CanonStateSubscriptions};
use reth_evm::ConfigureEvm;
use reth_metrics::Metrics;
use reth_primitives_traits::{AlloyBlockHeader, BlockTy, HeaderTy, NodePrimitives, ReceiptTy};
use reth_revm::cached::CachedReads;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use reth_tasks::TaskExecutor;
use std::{sync::Arc, time::Instant};
use tokio::sync::{oneshot, watch};
use tracing::*;

/// The `FlashBlockService` maintains an in-memory [`PendingFlashBlock`] built out of a sequence of
/// [`FlashBlock`]s.
#[derive(Debug)]
pub struct FlashBlockService<
    N: NodePrimitives,
    S,
    EvmConfig: ConfigureEvm<Primitives = N, NextBlockEnvCtx: From<OpFlashblockPayloadBase> + Unpin>,
    Provider,
> {
    /// Incoming flashblock stream.
    incoming_flashblock_rx: S,
    /// Signals when a block build is in progress.
    in_progress_tx: watch::Sender<Option<FlashBlockBuildInfo>>,
    /// Receiver for canonical chain update.
    canon_receiver: CanonStateNotifications<N>,
    /// Broadcast channel to forward received flashblocks from the subscription.
    received_flashblocks_tx: tokio::sync::broadcast::Sender<Arc<FlashBlock>>,

    /// Executes flashblock sequences to build pending blocks.
    builder: FlashBlockBuilder<EvmConfig, Provider>,
    /// Task executor for spawning block build jobs.
    spawner: TaskExecutor,
    /// Currently running block build job with start time and result receiver.
    job: Option<BuildJob<N>>,
    /// Manages flashblock sequences with caching and intelligent build selection.
    sequences: SequenceManager<N::SignedTx>,
    /// Tracks whether a rebuild is needed after state changes.
    ///
    /// Set to true when:
    /// - New flashblocks arrive (sequence updated)
    /// - Canonical tip changes (buildability may change)
    ///
    /// Set to false after `try_build()` executes to avoid redundant build attempts.
    rebuild: bool,

    /// `FlashBlock` service's metrics
    metrics: FlashBlockServiceMetrics,
}

impl<N, S, EvmConfig, Provider> FlashBlockService<N, S, EvmConfig, Provider>
where
    N: NodePrimitives,
    S: Stream<Item = eyre::Result<FlashBlock>> + Unpin + 'static,
    EvmConfig: ConfigureEvm<Primitives = N, NextBlockEnvCtx: From<OpFlashblockPayloadBase> + Unpin>
        + Clone
        + 'static,
    Provider: StateProviderFactory
        + CanonStateSubscriptions<Primitives = N>
        + BlockReaderIdExt<
            Header = HeaderTy<N>,
            Block = BlockTy<N>,
            Transaction = N::SignedTx,
            Receipt = ReceiptTy<N>,
        > + Unpin
        + Clone
        + 'static,
{
    /// Constructs a new `FlashBlockService` that receives [`FlashBlock`]s from `rx` stream.
    pub fn new(
        incoming_flashblock_rx: S,
        evm_config: EvmConfig,
        provider: Provider,
        spawner: TaskExecutor,
        compute_state_root: bool,
    ) -> Self {
        let (in_progress_tx, _) = watch::channel(None);
        let (received_flashblocks_tx, _) = tokio::sync::broadcast::channel(128);
        Self {
            incoming_flashblock_rx,
            in_progress_tx,
            canon_receiver: provider.subscribe_to_canonical_state(),
            received_flashblocks_tx,
            builder: FlashBlockBuilder::new(evm_config, provider),
            spawner,
            job: None,
            sequences: SequenceManager::new(compute_state_root),
            rebuild: false,
            metrics: FlashBlockServiceMetrics::default(),
        }
    }

    /// Returns the sender half to the received flashblocks.
    pub const fn flashblocks_broadcaster(
        &self,
    ) -> &tokio::sync::broadcast::Sender<Arc<FlashBlock>> {
        &self.received_flashblocks_tx
    }

    /// Returns the sender half to the flashblock sequence.
    pub const fn block_sequence_broadcaster(
        &self,
    ) -> &tokio::sync::broadcast::Sender<FlashBlockCompleteSequence> {
        self.sequences.block_sequence_broadcaster()
    }

    /// Returns a subscriber to the flashblock sequence.
    pub fn subscribe_block_sequence(&self) -> FlashBlockCompleteSequenceRx {
        self.sequences.subscribe_block_sequence()
    }

    /// Returns a receiver that signals when a flashblock is being built.
    pub fn subscribe_in_progress(&self) -> InProgressFlashBlockRx {
        self.in_progress_tx.subscribe()
    }

    /// Drives the service and sends new blocks to the receiver.
    ///
    /// This is an event-driven loop that:
    /// 1. Completes build jobs and updates sequence manager with results
    /// 2. Batches incoming flashblocks before processing
    /// 3. Handles canonical tip updates and prefills cache
    /// 4. Attempts to build only after state changes
    ///
    /// Note: this should be spawned
    pub async fn run(mut self, tx: watch::Sender<Option<PendingFlashBlock<N>>>) {
        loop {
            tokio::select! {
                // Event 1: Build job completes
                Some(result) = async {
                    match self.job.as_mut() {
                        Some((_, rx)) => rx.await.ok(),
                        None => std::future::pending().await,
                    }
                } => {
                    let (start_time, _) = self.job.take().unwrap();
                    let _ = self.in_progress_tx.send(None);

                    if let Ok(Some((pending, cached_reads))) = result {
                        let parent_hash = pending.parent_hash();
                        self.sequences.on_build_complete(parent_hash, Some((pending.clone(), cached_reads)));

                        let elapsed = start_time.elapsed();
                        self.metrics.execution_duration.record(elapsed.as_secs_f64());

                        let _ = tx.send(Some(pending));
                    }

                    self.rebuild = false;
                }

                // Event 2: New flashblock arrives (batch process all ready flashblocks)
                result = self.incoming_flashblock_rx.next() => {
                    match result {
                        Some(Ok(flashblock)) => {
                            // Process this flashblock
                            self.notify_received_flashblock(&flashblock);
                            if flashblock.index == 0 {
                                self.metrics.last_flashblock_length.record(
                                    self.sequences.pending().count() as f64
                                );
                            }
                            if let Err(err) = self.sequences.insert_flashblock(flashblock) {
                                trace!(target: "flashblocks", %err, "Failed to insert flashblock");
                            } else {
                                self.rebuild = true;
                            }
                        }
                        Some(Err(err)) => {
                            warn!(target: "flashblocks", %err, "Error receiving flashblock");
                        }
                        None => {
                            warn!(target: "flashblocks", "Flashblock stream ended");
                            break;
                        }
                    }
                }

                // Event 3: New canonical tip
                Ok(state) = self.canon_receiver.recv() => {
                    if let Some(tip) = state.tip_checked() {
                        let mut cached = CachedReads::default();
                        let committed = state.committed();
                        let new_execution_outcome = committed.execution_outcome();
                        for (addr, acc) in new_execution_outcome.bundle_accounts_iter() {
                            if let Some(info) = acc.info.clone() {
                                let storage = acc.storage.iter()
                                    .map(|(key, slot)| (*key, slot.present_value))
                                    .collect();
                                cached.insert_account(addr, info, storage);
                            }
                        }
                        self.sequences.on_new_canonical_tip(tip.hash(), cached);
                        self.rebuild = true;
                    }
                }
            }

            if self.rebuild {
                self.try_build().await;
            }
        }
    }

    /// Notifies all subscribers about the received flashblock.
    fn notify_received_flashblock(&self, flashblock: &FlashBlock) {
        if self.received_flashblocks_tx.receiver_count() > 0 {
            let _ = self.received_flashblocks_tx.send(Arc::new(flashblock.clone()));
        }
    }

    /// Attempts to build a block if no job is currently running and a buildable sequence exists.
    async fn try_build(&mut self) {
        if self.job.is_some() {
            return; // Already building
        }

        let Some(latest) = self.builder.provider().latest_header().ok().flatten() else {
            return;
        };

        let Some(args) = self.sequences.next_buildable_args(latest.hash(), latest.timestamp())
        else {
            return; // Nothing buildable
        };

        // Spawn build job
        let fb_info = FlashBlockBuildInfo {
            parent_hash: args.base.parent_hash,
            index: args.last_flashblock_index,
            block_number: args.base.block_number,
        };
        let _ = self.in_progress_tx.send(Some(fb_info));

        let (tx, rx) = oneshot::channel();
        let builder = self.builder.clone();
        self.spawner.spawn_blocking(Box::pin(async move {
            let _ = tx.send(builder.execute(args));
        }));
        self.job = Some((Instant::now(), rx));
    }
}

/// Information for a flashblock currently built
#[derive(Debug, Clone, Copy)]
pub struct FlashBlockBuildInfo {
    /// Parent block hash
    pub parent_hash: B256,
    /// Flashblock index within the current block's sequence
    pub index: u64,
    /// Block number of the flashblock being built.
    pub block_number: u64,
}

type BuildJob<N> =
    (Instant, oneshot::Receiver<eyre::Result<Option<(PendingFlashBlock<N>, CachedReads)>>>);

#[derive(Metrics)]
#[metrics(scope = "flashblock_service")]
struct FlashBlockServiceMetrics {
    /// The last complete length of flashblocks per block.
    last_flashblock_length: Histogram,
    /// The duration applying flashblock state changes in seconds.
    execution_duration: Histogram,
}
