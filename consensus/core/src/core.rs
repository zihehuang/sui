// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use consensus_config::{AuthorityIndex, ProtocolKeyPair};
use mysten_metrics::monitored_scope;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use crate::error::{ConsensusError, ConsensusResult};
use crate::{
    block::{
        timestamp_utc_ms, Block, BlockAPI, BlockRef, BlockTimestampMs, BlockV1, Round, SignedBlock,
        VerifiedBlock,
    },
    block_manager::BlockManager,
    commit_observer::CommitObserver,
    context::Context,
    storage::Store,
    threshold_clock::ThresholdClock,
    transaction::TransactionConsumer,
};

#[allow(dead_code)]
pub(crate) struct Core {
    context: Arc<Context>,
    /// The threshold clock that is used to keep track of the current round
    threshold_clock: ThresholdClock,
    /// The last produced block
    last_proposed_block: VerifiedBlock,
    /// The consumer to use in order to pull transactions to be included for the next proposals
    transaction_consumer: TransactionConsumer,
    /// The pending ancestors to be included in proposals organised by round.
    pending_ancestors: BTreeMap<Round, Vec<VerifiedBlock>>,
    /// The block manager which is responsible for keeping track of the DAG dependencies when processing new blocks
    /// and accept them or suspend if we are missing their causal history
    block_manager: BlockManager,
    // The commit observer is responsible for observing the commits and collecting
    // + sending subdags over the consensus output channel.
    commit_observer: CommitObserver,
    /// Sender of outgoing signals from Core.
    signals: CoreSignals,
    /// The keypair to be used for block signing
    block_signer: ProtocolKeyPair,
    /// The node's storage
    store: Arc<dyn Store>,
}

#[allow(dead_code)]
impl Core {
    pub(crate) fn new(
        context: Arc<Context>,
        transaction_consumer: TransactionConsumer,
        block_manager: BlockManager,
        commit_observer: CommitObserver,
        signals: CoreSignals,
        block_signer: ProtocolKeyPair,
        store: Arc<dyn Store>,
    ) -> Self {
        let (my_genesis_block, all_genesis_blocks) = Block::genesis(context.clone());

        Self {
            context: context.clone(),
            threshold_clock: ThresholdClock::new(0, context),
            last_proposed_block: my_genesis_block,
            transaction_consumer,
            pending_ancestors: BTreeMap::new(),
            block_manager,
            commit_observer,
            signals,
            block_signer,
            store,
        }
        .recover(all_genesis_blocks)
    }

    fn recover(mut self, genesis_blocks: Vec<VerifiedBlock>) -> Self {
        // We always need the genesis blocks as a starter point since we might not have advanced yet at all.
        let mut all_blocks = genesis_blocks;

        // Now fetch the proposed blocks per authority for their last two rounds.
        let context = self.context.clone();
        for (index, _authority) in context.committee.authorities() {
            let blocks = self
                .store
                .scan_last_blocks_by_author(index, 2)
                .expect("Storage error while recovering Core");
            all_blocks.extend(blocks);
        }

        // Recover the last proposed block
        self.last_proposed_block = all_blocks
            .iter()
            .filter(|block| block.author() == context.own_index)
            .max_by_key(|block| block.round())
            .cloned()
            .expect("At least one block - even genesis - should be present");

        // Accept all blocks but make sure that only the last quorum round blocks and onwards are kept.
        self.add_accepted_blocks(all_blocks, Some(0));
        self
    }

    /// Processes the provided blocks and accepts them if possible when their causal history exists.
    /// The method returns the references of parents that are unknown and need to be fetched.
    pub(crate) fn add_blocks(&mut self, blocks: Vec<VerifiedBlock>) -> BTreeSet<BlockRef> {
        let _scope = monitored_scope("Core::add_blocks");

        // Try to accept them via the block manager
        let (accepted_blocks, missing_blocks) = self
            .block_manager
            .try_accept_blocks(blocks)
            .unwrap_or_else(|err| panic!("Fatal error while accepting blocks: {err}"));

        // Now process them, basically move the threshold clock and add them to pending list
        self.add_accepted_blocks(accepted_blocks, None);

        // Attempt to create a new block and broadcast it.
        if let Some(block) = self.try_new_block(false) {
            if let Err(e) = self.signals.new_block(block.clone()) {
                warn!("Failed to broadcast block {}: {:?}", block, e);
                // TODO: propagate shutdown or ensure this will never return error?
            }
        }

        missing_blocks
    }

    /// Adds/processed all the newly `accepted_blocks`. We basically try to move the threshold clock and add them to the
    /// pending ancestors list. The `pending_ancestors_retain_rounds` if defined then the method will retain on the pending ancestors
    /// only the `pending_ancestors_retain_rounds` from the last formed quorum round. For example if set to zero (0), then
    /// we'll strictly keep in the pending ancestors list the blocks of round >= last_quorum_round. If not defined, so None
    /// is provided, then all the pending ancestors will be kep until the next block proposal.
    fn add_accepted_blocks(
        &mut self,
        accepted_blocks: Vec<VerifiedBlock>,
        pending_ancestors_retain_rounds: Option<u32>,
    ) {
        // Advance the threshold clock. If advanced to a new round then send a signal that a new quorum has been received.
        if let Some(new_round) = self
            .threshold_clock
            .add_blocks(accepted_blocks.iter().map(|b| b.reference()).collect())
        {
            // notify that threshold clock advanced to new round
            let _ = self.signals.new_round(new_round);
            // TODO: propagate shutdown or ensure this will never return error?
        }

        // Report the threshold clock round
        self.context
            .metrics
            .node_metrics
            .threshold_clock_round
            .set(self.threshold_clock.get_round() as i64);

        for accepted_block in accepted_blocks {
            self.pending_ancestors
                .entry(accepted_block.round())
                .or_default()
                .push(accepted_block);
        }

        // TODO: we might need to consider the following:
        // 1. Add some sort of protection from bulk catch ups - or intentional validator attack - that is flooding us with
        // many blocks, so we don't spam the pending_ancestors list and OOM
        // 2. Probably it doesn't make much sense to keep blocks around from too many rounds ago to reference as the data
        // might not be relevant any more.
        if let Some(retain_ancestor_rounds_from_quorum) = pending_ancestors_retain_rounds {
            let last_quorum = self.threshold_clock.get_round().saturating_sub(1);
            self.pending_ancestors.retain(|round, _| {
                *round >= last_quorum.saturating_sub(retain_ancestor_rounds_from_quorum)
            });
        }
    }

    /// Force creating a new block for the dictated round. This is used when a leader timeout occurs.
    pub(crate) fn force_new_block(&mut self, round: Round) -> Option<VerifiedBlock> {
        if self.last_proposed_round() < round {
            self.context.metrics.node_metrics.leader_timeout_total.inc();
            if let Some(block) = self.try_new_block(true) {
                if let Err(e) = self.signals.new_block(block.clone()) {
                    warn!("Failed to broadcast block {}: {:?}", block, e);
                    // TODO: propagate shutdown or ensure this will never return error?
                }
                return Some(block);
            }
        }
        None
    }

    /// Attempts to propose a new block for the next round. If a block has already proposed for latest
    /// or earlier round, then no block is created and None is returned.
    fn try_new_block(&mut self, ignore_leaders_check: bool) -> Option<VerifiedBlock> {
        let _scope = monitored_scope("Core::try_new_block");

        let clock_round = self.threshold_clock.get_round();
        if clock_round <= self.last_proposed_round() {
            return None;
        }

        // create a new block either because we want to "forcefully" propose a block due to a leader timeout,
        // or because we are actually ready to produce the block (leader exists)
        if ignore_leaders_check || self.last_quorum_leaders_exist() {
            // TODO: produce the block for the clock_round. As the threshold clock can advance many rounds at once (ex
            // because we synchronized a bulk of blocks) we can decide here whether we want to produce blocks per round
            // or just the latest one. From earlier experiments I saw only benefit on proposing for the penultimate round
            // only when the validator was supposed to be the leader of the round - so we bring down the missed leaders.
            // Probably proposing for all the intermediate rounds might not make much sense.

            // 1. Consume the ancestors to be included in proposal
            let now = timestamp_utc_ms();
            let ancestors = self.ancestors_to_propose(clock_round, now);

            //2. consume the next transactions to be included.
            let payload = self.transaction_consumer.next();

            //3. create the block and insert to storage.
            // TODO: take a decision on whether we want to flush to disk at this point the DagState.

            // TODO: this will be refactored once the signing path/approach has been introduced. Adding as is for now
            // to keep things rolling in the implementation.
            let block = Block::V1(BlockV1::new(
                self.context.committee.epoch(),
                clock_round,
                self.context.own_index,
                now,
                ancestors,
                payload,
            ));
            let signed_block =
                SignedBlock::new(block, &self.block_signer).expect("Block signing failed.");
            let serialized = signed_block
                .serialize()
                .expect("Block serialization failed.");
            // Unnecessary to verify own blocks.
            let verified_block = VerifiedBlock::new_verified(signed_block, serialized);

            //4. Add to the threshold clock
            self.threshold_clock.add_block(verified_block.reference());

            // Add to the pending ancestors
            self.pending_ancestors
                .entry(verified_block.round())
                .or_default()
                .push(verified_block.clone());

            let (accepted_blocks, missing) = self
                .block_manager
                .try_accept_blocks(vec![verified_block.clone()])
                .unwrap_or_else(|err| panic!("Fatal error while accepting our own block: {err}"));
            assert_eq!(accepted_blocks.len(), 1);
            assert!(missing.is_empty());

            self.last_proposed_block = verified_block.clone();

            tracing::debug!("New block created {}", verified_block);

            //5. emit an event that a new block is ready
            let _ = self.signals.new_block_ready(verified_block.reference());
            // TODO: propagate shutdown or ensure this will never return error?

            return Some(verified_block);
        }

        None
    }

    /// Retrieves the next ancestors to propose to form a block at `clock_round` round. Also the `block_timestamp` is provided
    /// to sanity check that everything that goes into the proposal is ensured to have a timestamp < block_timestamp
    fn ancestors_to_propose(
        &mut self,
        clock_round: Round,
        block_timestamp: BlockTimestampMs,
    ) -> Vec<BlockRef> {
        // Now take all the ancestors up to the clock_round (excluded) and then remove them from the map.
        let ancestors = self
            .pending_ancestors
            .range(0..clock_round)
            .flat_map(|(_, blocks)| blocks)
            .collect::<Vec<_>>();

        // Ensure that timestamps are correct
        ancestors.iter().for_each(|block|{
            // We assume that our system's clock can't go backwards when we perform the check here (ex due to ntp corrections)
            assert!(block.timestamp_ms() <= block_timestamp, "Violation, ancestor block timestamp {} greater than our timestamp {block_timestamp}", block.timestamp_ms());
        });

        // Compress the references in the block. We don't want to include an ancestors that already referenced by other blocks
        // we are about to include.
        let all_ancestors_parents: HashSet<&BlockRef> = ancestors
            .iter()
            .flat_map(|block| block.ancestors())
            .collect();

        let mut to_propose = HashSet::new();
        for block in ancestors.into_iter() {
            if !all_ancestors_parents.contains(&block.reference()) {
                to_propose.insert(block.reference());
            }
        }

        // always include our last block to ensure that is not somehow excluded by the DAG compression
        to_propose.insert(self.last_proposed_block.reference());

        assert!(!to_propose.is_empty());

        // Now clean up the pending ancestors
        self.pending_ancestors
            .retain(|round, _blocks| *round >= clock_round);

        to_propose.into_iter().collect()
    }

    /// Checks whether all the leaders of the previous quorum exist.
    /// TODO: we can leverage some additional signal here in order to more cleverly manipulate later the leader timeout
    /// Ex if we already have one leader - the first in order - we might don't want to wait as much.
    fn last_quorum_leaders_exist(&self) -> bool {
        // TODO: check that we are ready to produce a new block. This will mainly check that the leaders of the previous
        // quorum exist.
        let quorum_round = self.threshold_clock.get_round().saturating_sub(1);

        let leaders = self.leaders(quorum_round);
        if let Some(ancestors) = self.pending_ancestors.get(&quorum_round) {
            // Search for all the leaders. If at least one is not found, then return false.
            // A linear search should be fine here as the set of elements is not expected to be small enough and more sophisticated
            // data structures might not give us much here.
            return leaders.iter().all(|leader| {
                ancestors
                    .iter()
                    .any(|entry| entry.reference().author == *leader)
            });
        }
        false
    }

    /// Returns the leaders of the provided round.
    fn leaders(&self, round: Round) -> Vec<AuthorityIndex> {
        // TODO: this info will be retrieved from the base committers. For now just do a simple round robin so we can
        // use it in tests.
        vec![AuthorityIndex::new_for_test(
            round % self.context.committee.size() as u32,
        )]
    }

    fn last_proposed_round(&self) -> Round {
        self.last_proposed_block.round()
    }

    fn last_proposed_block(&self) -> &VerifiedBlock {
        &self.last_proposed_block
    }
}

/// Senders of signals from Core, for outputs and events (ex new block produced).
#[allow(dead_code)]
pub(crate) struct CoreSignals {
    tx_block_broadcast: broadcast::Sender<VerifiedBlock>,
    new_round_sender: watch::Sender<Round>,
    block_ready_sender: watch::Sender<Option<BlockRef>>,
}

impl CoreSignals {
    // TODO: move to Parameters.
    const BROADCAST_BACKLOG_CAPACITY: usize = 1000;

    #[allow(dead_code)]
    pub fn new() -> (Self, CoreSignalsReceivers) {
        let (tx_block_broadcast, _rx_block_broadcast) =
            broadcast::channel::<VerifiedBlock>(Self::BROADCAST_BACKLOG_CAPACITY);
        let (block_ready_sender, block_ready_receiver) = watch::channel(None);
        let (new_round_sender, new_round_receiver) = watch::channel(0);

        let me = Self {
            tx_block_broadcast: tx_block_broadcast.clone(),
            block_ready_sender,
            new_round_sender,
        };

        let receivers = CoreSignalsReceivers {
            tx_block_broadcast,
            block_ready_receiver,
            new_round_receiver,
        };

        (me, receivers)
    }

    /// Sends a signal to all the waiters that a new block has been produced.
    pub fn new_block(&self, block: VerifiedBlock) -> ConsensusResult<()> {
        self.tx_block_broadcast
            .send(block)
            .map_err(|_| ConsensusError::Shutdown)?;
        Ok(())
    }

    /// Sends a signal to all the waiters that a new block has been produced.
    pub fn new_block_ready(&mut self, block: BlockRef) -> ConsensusResult<()> {
        self.block_ready_sender
            .send(Some(block))
            .map_err(|_| ConsensusError::Shutdown)
    }

    /// Sends a signal that threshold clock has advanced to new round. The `round_number` is the round at which the
    /// threshold clock has advanced to.
    pub fn new_round(&mut self, round_number: Round) -> ConsensusResult<()> {
        self.new_round_sender
            .send(round_number)
            .map_err(|_| ConsensusError::Shutdown)
    }
}

/// Receivers of signals from Core.
/// Intentially un-clonable. Comonents should only subscribe to channels they need.
pub(crate) struct CoreSignalsReceivers {
    tx_block_broadcast: broadcast::Sender<VerifiedBlock>,
    #[allow(dead_code)]
    block_ready_receiver: watch::Receiver<Option<BlockRef>>,
    new_round_receiver: watch::Receiver<Round>,
}

impl CoreSignalsReceivers {
    #[allow(dead_code)]
    pub(crate) fn block_broadcast_receiver(&self) -> broadcast::Receiver<VerifiedBlock> {
        self.tx_block_broadcast.subscribe()
    }

    #[allow(dead_code)]
    pub(crate) fn block_ready_receiver(&self) -> watch::Receiver<Option<BlockRef>> {
        self.block_ready_receiver.clone()
    }

    pub(crate) fn new_round_receiver(&self) -> watch::Receiver<Round> {
        self.new_round_receiver.clone()
    }
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeSet, time::Duration};

    use consensus_config::{local_committee_and_keys, Stake};
    use parking_lot::RwLock;
    use sui_protocol_config::ProtocolConfig;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::{
        block::TestBlock, dag_state::DagState, storage::mem_store::MemStore,
        transaction::TransactionClient,
    };

    /// Recover Core and continue proposing from the last round which forms a quorum.
    #[tokio::test]
    async fn test_core_recover_from_store_for_full_round() {
        let (context, mut key_pairs) = Context::new_for_test(4);
        let context = Arc::new(context);
        let store = Arc::new(MemStore::new());
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));
        let block_manager = BlockManager::new(context.clone(), dag_state.clone());
        let (_transaction_client, tx_receiver) = TransactionClient::new(context.clone());
        let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);

        let (sender, _receiver) = unbounded_channel();
        let commit_observer = CommitObserver::new(
            context.clone(),
            sender.clone(),
            0, // last_processed_index
            dag_state.clone(),
            store.clone(),
        );

        // Create test blocks for all the authorities for 4 rounds and populate them in store
        let (_, mut last_round_blocks) = Block::genesis(context.clone());
        let mut all_blocks: Vec<VerifiedBlock> = last_round_blocks.clone();
        for round in 1..=4 {
            let mut this_round_blocks = Vec::new();
            for (index, _authority) in context.committee.authorities() {
                let block = TestBlock::new(round, index.value() as u32)
                    .set_ancestors(last_round_blocks.iter().map(|b| b.reference()).collect())
                    .build();

                this_round_blocks.push(VerifiedBlock::new_for_test(block));
            }
            all_blocks.extend(this_round_blocks.clone());
            last_round_blocks = this_round_blocks;
        }

        // write them in store
        store.write(all_blocks, vec![]).expect("Storage error");

        // Now spin up core
        let (signals, signal_receivers) = CoreSignals::new();
        let mut core = Core::new(
            context.clone(),
            transaction_consumer,
            block_manager,
            commit_observer,
            signals,
            key_pairs.remove(context.own_index.value()).1,
            store,
        );

        // New round should be 5
        let mut new_round = signal_receivers.new_round_receiver();
        assert_eq!(*new_round.borrow_and_update(), 5);

        // When trying to propose now we should propose block for round 5
        let proposed_block = core
            .try_new_block(true)
            .expect("A block should have been created");
        assert_eq!(proposed_block.round(), 5);
        let ancestors = proposed_block.ancestors();

        // Only ancestors of round 4 should be included.
        assert_eq!(ancestors.len(), 4);
        for ancestor in ancestors {
            assert_eq!(ancestor.round, 4);
        }
    }

    /// Recover Core and continue proposing when having a partial last round which doesn't form a quorum and we haven't
    /// proposed for that round yet.
    #[tokio::test]
    async fn test_core_recover_from_store_for_partial_round() {
        let (context, mut key_pairs) = Context::new_for_test(4);
        let context = Arc::new(context);
        let store = Arc::new(MemStore::new());
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));
        let block_manager = BlockManager::new(context.clone(), dag_state.clone());
        let (_transaction_client, tx_receiver) = TransactionClient::new(context.clone());
        let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);

        let (sender, _receiver) = unbounded_channel();
        let commit_observer = CommitObserver::new(
            context.clone(),
            sender.clone(),
            0, // last_processed_index
            dag_state.clone(),
            store.clone(),
        );

        // Create test blocks for all authorities except our's (index = 0) .
        let (_, mut last_round_blocks) = Block::genesis(context.clone());
        let mut all_blocks = last_round_blocks.clone();
        for round in 1..=4 {
            let mut this_round_blocks = Vec::new();

            // For round 4 only produce f+1 blocks only skip our validator and that of position 1 from creating blocks.
            let authorities_to_skip = if round == 4 {
                context.committee.validity_threshold() as usize
            } else {
                // otherwise always skip creating a block for our authority
                1
            };

            for (index, _authority) in context.committee.authorities().skip(authorities_to_skip) {
                let block = TestBlock::new(round, index.value() as u32)
                    .set_ancestors(last_round_blocks.iter().map(|b| b.reference()).collect())
                    .build();
                this_round_blocks.push(VerifiedBlock::new_for_test(block));
            }
            all_blocks.extend(this_round_blocks.clone());
            last_round_blocks = this_round_blocks;
        }

        // write them in store
        store.write(all_blocks, vec![]).expect("Storage error");

        // Now spin up core
        let (signals, signal_receivers) = CoreSignals::new();
        let mut core = Core::new(
            context.clone(),
            transaction_consumer,
            block_manager,
            commit_observer,
            signals,
            key_pairs.remove(context.own_index.value()).1,
            store,
        );

        // New round should be 4
        let mut new_round = signal_receivers.new_round_receiver();
        assert_eq!(*new_round.borrow_and_update(), 4);

        // When trying to propose now we should propose block for round 4
        let proposed_block = core
            .try_new_block(true)
            .expect("A block should have been created");
        assert_eq!(proposed_block.round(), 4);
        let ancestors = proposed_block.ancestors();

        assert_eq!(ancestors.len(), 4);
        for ancestor in ancestors {
            if ancestor.author == context.own_index {
                assert_eq!(ancestor.round, 0);
            } else {
                assert_eq!(ancestor.round, 3);
            }
        }
    }

    #[tokio::test]
    async fn test_core_propose_after_genesis() {
        let _guard = ProtocolConfig::apply_overrides_for_testing(|_, mut config| {
            config.set_consensus_max_transaction_size_bytes(2_000);
            config.set_consensus_max_transactions_in_block_bytes(2_000);
            config
        });

        let (context, mut key_pairs) = Context::new_for_test(4);
        let context = Arc::new(context);
        let store = Arc::new(MemStore::new());
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));

        let block_manager = BlockManager::new(context.clone(), dag_state.clone());
        let (transaction_client, tx_receiver) = TransactionClient::new(context.clone());
        let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);
        let (signals, _signal_receivers) = CoreSignals::new();

        let (sender, _receiver) = unbounded_channel();
        let commit_observer = CommitObserver::new(
            context.clone(),
            sender.clone(),
            0, // last_processed_index
            dag_state.clone(),
            store.clone(),
        );
        let mut core = Core::new(
            context.clone(),
            transaction_consumer,
            block_manager,
            commit_observer,
            signals,
            key_pairs.remove(context.own_index.value()).1,
            store,
        );

        // Send some transactions
        let mut total = 0;
        let mut index = 0;
        loop {
            let transaction =
                bcs::to_bytes(&format!("Transaction {index}")).expect("Shouldn't fail");
            total += transaction.len();
            index += 1;
            transaction_client.submit(transaction).await.unwrap();

            // Create total size of transactions up to 1KB
            if total >= 1_000 {
                break;
            }
        }

        // trigger the try_new_block - that should return now a new block
        let block = core
            .try_new_block(false)
            .expect("A new block should have been created");

        // A new block created - assert the details
        assert_eq!(block.round(), 1);
        assert_eq!(block.author().value(), 0);
        assert_eq!(block.ancestors().len(), 4);

        let mut total = 0;
        for (i, transaction) in block.transactions().iter().enumerate() {
            total += transaction.data().len() as u64;
            let transaction: String = bcs::from_bytes(transaction.data()).unwrap();
            assert_eq!(format!("Transaction {i}"), transaction);
        }
        assert!(
            total
                <= context
                    .protocol_config
                    .consensus_max_transactions_in_block_bytes()
        );

        // genesis blocks should be referenced
        let (_genesis_my, all_genesis) = Block::genesis(context);

        for ancestor in block.ancestors() {
            all_genesis
                .iter()
                .find(|block| block.reference() == *ancestor)
                .expect("Block should be found amongst genesis blocks");
        }

        // Try to propose again - with or without ignore leaders check, it will not return any block
        assert!(core.try_new_block(false).is_none());
        assert!(core.try_new_block(true).is_none());
    }

    #[tokio::test]
    async fn test_core_propose_once_receiving_a_quorum() {
        let (context, mut key_pairs) = Context::new_for_test(4);
        let context = Arc::new(context);

        let store = Arc::new(MemStore::new());
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));

        let block_manager = BlockManager::new(context.clone(), dag_state.clone());
        let (_transaction_client, tx_receiver) = TransactionClient::new(context.clone());
        let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);
        let (signals, _signal_receivers) = CoreSignals::new();

        let (sender, _receiver) = unbounded_channel();
        let commit_observer = CommitObserver::new(
            context.clone(),
            sender.clone(),
            0, // last_processed_index
            dag_state.clone(),
            store.clone(),
        );
        let mut core = Core::new(
            context.clone(),
            transaction_consumer,
            block_manager,
            commit_observer,
            signals,
            key_pairs.remove(context.own_index.value()).1,
            store,
        );

        let mut expected_ancestors = BTreeSet::new();

        // Adding one block now will trigger the creation of new block for round 1
        let block_1 = VerifiedBlock::new_for_test(TestBlock::new(1, 1).build());
        expected_ancestors.insert(block_1.reference());
        _ = core.add_blocks(vec![block_1]);

        assert_eq!(core.last_proposed_round(), 1);
        expected_ancestors.insert(core.last_proposed_block().reference());
        // attempt to create a block - none will be produced.
        assert!(core.try_new_block(false).is_none());

        // Adding another block now forms a quorum for round 1, so block at round 2 will proposed
        let block_3 = VerifiedBlock::new_for_test(TestBlock::new(1, 2).build());
        expected_ancestors.insert(block_3.reference());
        _ = core.add_blocks(vec![block_3]);

        assert_eq!(core.last_proposed_round(), 2);

        let proposed_block = core.last_proposed_block();
        assert_eq!(proposed_block.round(), 2);
        assert_eq!(proposed_block.author(), context.own_index);
        assert_eq!(proposed_block.ancestors().len(), 3);
        let ancestors = proposed_block.ancestors();
        let ancestors = ancestors.iter().cloned().collect::<BTreeSet<_>>();
        assert_eq!(ancestors, expected_ancestors);
    }

    #[tokio::test]
    async fn test_core_try_new_block_leader_timeout() {
        // Create the cores for all authorities
        let cores = create_cores(vec![1, 1, 1, 1]);

        // Create blocks for rounds 1..=3 from all Cores except Core of authority 3, so we miss the block from it. As
        // it will be the leader of round 3 then no-one will be able to progress to round 4 unless we explicitly trigger
        // the block creation.
        // create the cores and their signals for all the authorities
        let mut cores = cores.into_iter().take(3).collect::<Vec<_>>();

        // Now iterate over a few rounds and ensure the corresponding signals are created while network advances
        let mut last_round_blocks = Vec::new();
        for round in 1..=3 {
            let mut this_round_blocks = Vec::new();

            for (core, _signal_receivers) in &mut cores {
                core.add_blocks(last_round_blocks.clone());

                assert_eq!(core.last_proposed_round(), round);

                this_round_blocks.push(core.last_proposed_block.clone());
            }

            last_round_blocks = this_round_blocks;
        }

        // Try to create the blocks for round 4 by calling the try_new_block method. No block should be created as the
        // leader - authority 3 - hasn't proposed any block.
        for (core, _) in &mut cores {
            core.add_blocks(last_round_blocks.clone());
            assert!(core.try_new_block(false).is_none());
        }

        // Now try to create the blocks for round 4 via the leader timeout method which should ignore any leader checks
        for (core, _) in &mut cores {
            assert!(core.force_new_block(4).is_some());
            assert_eq!(core.last_proposed_round(), 4);
        }
    }

    #[tokio::test]
    async fn test_core_signals() {
        // create the cores and their signals for all the authorities
        let mut cores = create_cores(vec![1, 1, 1, 1]);

        // Now iterate over a few rounds and ensure the corresponding signals are created while network advances
        let mut last_round_blocks = Vec::new();
        for round in 1..=10 {
            let mut this_round_blocks = Vec::new();

            for (core, signal_receivers) in &mut cores {
                // add the blocks from last round
                // this will trigger a block creation for the round and a signal should be emitted
                core.add_blocks(last_round_blocks.clone());

                // A "new round" signal should be received given that all the blocks of previous round have been processed
                let new_round = receive(
                    Duration::from_secs(1),
                    signal_receivers.new_round_receiver(),
                )
                .await;
                assert_eq!(new_round, round);

                // Check that a new block has been proposed
                let block_ref = receive(
                    Duration::from_secs(1),
                    signal_receivers.block_ready_receiver(),
                )
                .await
                .unwrap();
                assert_eq!(block_ref.round, round);
                assert_eq!(block_ref.author, core.context.own_index);

                // append the new block to this round blocks
                this_round_blocks.push(core.last_proposed_block().clone());

                let block = core.last_proposed_block();

                // ensure that produced block is referring to the blocks of last_round
                assert_eq!(block.ancestors().len(), core.context.committee.size());
                for ancestor in block.ancestors() {
                    if block.round() > 1 {
                        // don't bother with round 1 block which just contains the genesis blocks.
                        assert!(
                            last_round_blocks
                                .iter()
                                .any(|block| block.reference() == *ancestor),
                            "Reference from previous round should be added"
                        );
                    }
                }
            }

            last_round_blocks = this_round_blocks;
        }
    }

    #[tokio::test]
    async fn test_core_compress_proposal_references() {
        // create the cores and their signals for all the authorities
        let mut cores = create_cores(vec![1, 1, 1, 1]);

        let mut last_round_blocks = Vec::new();
        let mut all_blocks = Vec::new();

        let excluded_authority = AuthorityIndex::new_for_test(3);

        for round in 1..=10 {
            let mut this_round_blocks = Vec::new();

            for (core, _) in &mut cores {
                // do not produce any block for authority 3
                if core.context.own_index == excluded_authority {
                    continue;
                }

                // try to propose to ensure that we are covering the case where we miss the leader authority 3
                core.add_blocks(last_round_blocks.clone());
                core.force_new_block(round);

                let block = core.last_proposed_block();
                assert_eq!(block.round(), round);

                // append the new block to this round blocks
                this_round_blocks.push(block.clone());
            }

            last_round_blocks = this_round_blocks.clone();
            all_blocks.extend(this_round_blocks);
        }

        // Now send all the produced blocks to core of authority 3. It should produce a new block. If no compression would
        // be applied the we should expect all the previous blocks to be referenced from round 0..=10. However, since compression
        // is applied only the last round's (10) blocks should be referenced + the authority's block of round 0.
        let (core, _) = &mut cores[excluded_authority];
        core.add_blocks(all_blocks);

        // Assert that a block has been created for round 11 and it references to blocks of round 10 for the other peers, and
        // to round 0 for its own block.
        let block = core.last_proposed_block();
        assert_eq!(block.round(), 11);
        assert_eq!(block.ancestors().len(), 4);
        for block_ref in block.ancestors() {
            if block_ref.author == excluded_authority {
                assert_eq!(block_ref.round, 0);
            } else {
                assert_eq!(block_ref.round, 10);
            }
        }
    }

    /// Creates cores for the specified number of authorities for their corresponding stakes. The method returns the
    /// cores and their respective signal receivers are returned in `AuthorityIndex` order asc.
    fn create_cores(authorities: Vec<Stake>) -> Vec<(Core, CoreSignalsReceivers)> {
        let mut cores = Vec::new();

        for index in 0..authorities.len() {
            let (committee, mut signers) = local_committee_and_keys(0, authorities.clone());
            let (mut context, _) = Context::new_for_test(4);
            context = context
                .with_committee(committee)
                .with_authority_index(AuthorityIndex::new_for_test(index as u32));

            let context = Arc::new(context);
            let store = Arc::new(MemStore::new());
            let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));

            let block_manager = BlockManager::new(context.clone(), dag_state.clone());
            let (_transaction_client, tx_receiver) = TransactionClient::new(context.clone());
            let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);
            let (signals, signal_receivers) = CoreSignals::new();

            let (sender, _receiver) = unbounded_channel();
            let commit_observer = CommitObserver::new(
                context.clone(),
                sender.clone(),
                0, // last_processed_index
                dag_state.clone(),
                store.clone(),
            );
            let block_signer = signers.remove(index).1;

            let core = Core::new(
                context,
                transaction_consumer,
                block_manager,
                commit_observer,
                signals,
                block_signer,
                store,
            );

            cores.push((core, signal_receivers));
        }
        cores
    }

    async fn receive<T: Copy>(timeout: Duration, mut receiver: watch::Receiver<T>) -> T {
        tokio::time::timeout(timeout, receiver.changed())
            .await
            .expect("Timeout while waiting to read from receiver")
            .expect("Signal receive channel shouldn't be closed");
        *receiver.borrow_and_update()
    }
}
