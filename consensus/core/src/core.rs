// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
    time::Duration,
};

use consensus_config::ProtocolKeyPair;
use mysten_metrics::monitored_scope;
use parking_lot::RwLock;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use crate::stake_aggregator::{QuorumThreshold, StakeAggregator};
use crate::{
    block::{
        timestamp_utc_ms, Block, BlockAPI, BlockRef, BlockTimestampMs, BlockV1, Round, SignedBlock,
        Slot, VerifiedBlock,
    },
    block_manager::BlockManager,
    commit_observer::CommitObserver,
    context::Context,
    dag_state::DagState,
    error::{ConsensusError, ConsensusResult},
    threshold_clock::ThresholdClock,
    transaction::TransactionConsumer,
    universal_committer::{
        universal_committer_builder::UniversalCommitterBuilder, UniversalCommitter,
    },
    CommittedSubDag,
};

// TODO: Move to protocol config once initial value is finalized.
const NUM_LEADERS_PER_ROUND: usize = 1;

// Maximum number of commit votes to include in a block.
// TODO: Move to protocol config, and verify in BlockVerifier.
const MAX_COMMIT_VOTES_PER_BLOCK: usize = 100;

pub(crate) struct Core {
    context: Arc<Context>,
    /// The threshold clock that is used to keep track of the current round
    threshold_clock: ThresholdClock,
    /// The consumer to use in order to pull transactions to be included for the next proposals
    transaction_consumer: TransactionConsumer,
    /// The block manager which is responsible for keeping track of the DAG dependencies when processing new blocks
    /// and accept them or suspend if we are missing their causal history
    block_manager: BlockManager,
    /// Used to make commit decisions for leader blocks in the dag.
    committer: UniversalCommitter,
    /// The last produced block
    last_proposed_block: VerifiedBlock,
    /// The blocks of the last included ancestors per authority. This vector is basically used as a
    /// watermark in order to include in the next block proposal only ancestors of higher rounds.
    /// By default, is initialised with `None` values.
    last_included_ancestors: Vec<Option<BlockRef>>,
    /// The last decided leader returned from the universal committer. Important to note
    /// that this does not signify that the leader has been persisted yet as it still has
    /// to go through CommitObserver and persist the commit in store. On recovery/restart
    /// the last_decided_leader will be set to the last_commit leader in dag state.
    last_decided_leader: Slot,
    /// The commit observer is responsible for observing the commits and collecting
    /// + sending subdags over the consensus output channel.
    commit_observer: CommitObserver,
    /// Sender of outgoing signals from Core.
    signals: CoreSignals,
    /// The keypair to be used for block signing
    block_signer: ProtocolKeyPair,
    /// Keeping track of state of the DAG, including blocks, commits and last committed rounds.
    dag_state: Arc<RwLock<DagState>>,
}

impl Core {
    pub(crate) fn new(
        context: Arc<Context>,
        transaction_consumer: TransactionConsumer,
        block_manager: BlockManager,
        commit_observer: CommitObserver,
        signals: CoreSignals,
        block_signer: ProtocolKeyPair,
        dag_state: Arc<RwLock<DagState>>,
    ) -> Self {
        let last_decided_leader = dag_state.read().last_commit_leader();

        let committer = UniversalCommitterBuilder::new(context.clone(), dag_state.clone())
            .with_number_of_leaders(NUM_LEADERS_PER_ROUND)
            .with_pipeline(true)
            .build();

        // Recover the last proposed block
        let last_proposed_block = dag_state
            .read()
            .get_last_block_for_authority(context.own_index);

        // Recover the last included ancestor rounds based on the last proposed block. That will allow
        // to perform the next block proposal by using ancestor blocks of higher rounds and avoid
        // re-including blocks that have been already included in the last (or earlier) block proposal.
        // This is only strongly guaranteed for a quorum of ancestors. It is still possible to re-include
        // a block from an authority which hadn't been added as part of the last proposal hence its
        // latest included ancestor is not accurately captured here. This is considered a small deficiency,
        // and it mostly matters just for this next proposal without any actual penalties in performance
        // or block proposal.
        let mut last_included_ancestors = vec![None; context.committee.size()];
        for ancestor in last_proposed_block.ancestors() {
            last_included_ancestors[ancestor.author] = Some(*ancestor);
        }

        Self {
            context: context.clone(),
            threshold_clock: ThresholdClock::new(0, context.clone()),
            last_proposed_block,
            transaction_consumer,
            last_included_ancestors,
            block_manager,
            committer,
            last_decided_leader,
            commit_observer,
            signals,
            block_signer,
            dag_state,
        }
        .recover()
    }

    fn recover(mut self) -> Self {
        // Recover the last available quorum to correctly advance the threshold clock.
        let last_quorum = self.dag_state.read().last_quorum();
        self.add_accepted_blocks(last_quorum);
        // Try to commit and propose, since they may not have run after the last storage write.
        self.try_commit().unwrap();
        self.try_propose(true).unwrap();

        self
    }

    /// Processes the provided blocks and accepts them if possible when their causal history exists.
    /// The method returns the references of parents that are unknown and need to be fetched.
    pub(crate) fn add_blocks(
        &mut self,
        blocks: Vec<VerifiedBlock>,
    ) -> ConsensusResult<BTreeSet<BlockRef>> {
        let _scope = monitored_scope("Core::add_blocks");

        // Try to accept them via the block manager
        let (accepted_blocks, missing_blocks) = self.block_manager.try_accept_blocks(blocks);

        if !accepted_blocks.is_empty() {
            // Now add accepted blocks to the threshold clock and pending ancestors list.
            self.add_accepted_blocks(accepted_blocks);

            self.try_commit()?;

            // Try to propose now since there are new blocks accepted.
            self.try_propose(false)?;
        }

        Ok(missing_blocks)
    }

    /// Adds/processed all the newly `accepted_blocks`. We basically try to move the threshold clock and add them to the
    /// pending ancestors list.
    fn add_accepted_blocks(&mut self, accepted_blocks: Vec<VerifiedBlock>) {
        // Advance the threshold clock. If advanced to a new round then send a signal that a new quorum has been received.
        if let Some(new_round) = self
            .threshold_clock
            .add_blocks(accepted_blocks.iter().map(|b| b.reference()).collect())
        {
            // notify that threshold clock advanced to new round
            self.signals.new_round(new_round);
        }

        // Report the threshold clock round
        self.context
            .metrics
            .node_metrics
            .threshold_clock_round
            .set(self.threshold_clock.get_round() as i64);
    }

    /// Force creating a new block for the dictated round. This is used when a leader timeout occurs.
    pub(crate) fn force_new_block(
        &mut self,
        round: Round,
    ) -> ConsensusResult<Option<VerifiedBlock>> {
        if self.last_proposed_round() < round {
            self.context.metrics.node_metrics.leader_timeout_total.inc();
            return self.try_propose(true);
        }
        Ok(None)
    }

    // Attempts to create a new block, persist and propose it to all peers.
    // When force is true, ignore if leader from the last round exists among ancestors and if
    // the minimum round delay has passed.
    fn try_propose(&mut self, force: bool) -> ConsensusResult<Option<VerifiedBlock>> {
        if let Some(block) = self.try_new_block(force) {
            // When there is only one authority in committee, it is unnecessary to broadcast
            // the block which will fail anyway without subscribers to the signal.
            if self.context.committee.size() > 1 {
                self.signals.new_block(block.clone())?;
            }
            // The new block may help commit.
            self.try_commit()?;
            return Ok(Some(block));
        }
        Ok(None)
    }

    /// Attempts to propose a new block for the next round. If a block has already proposed for latest
    /// or earlier round, then no block is created and None is returned.
    fn try_new_block(&mut self, force: bool) -> Option<VerifiedBlock> {
        let _scope = monitored_scope("Core::try_new_block");

        let clock_round = self.threshold_clock.get_round();
        if clock_round <= self.last_proposed_round() {
            return None;
        }

        let now = timestamp_utc_ms();

        // Create a new block either because we want to "forcefully" propose a block due to a leader timeout,
        // or because we are actually ready to produce the block (leader exists and min delay has passed).
        if !force {
            if !self.last_quorum_leaders_exist() {
                return None;
            }
            if Duration::from_millis(now.saturating_sub(self.last_proposed_timestamp_ms()))
                < self.context.parameters.min_round_delay
            {
                return None;
            }
        }

        // TODO: produce the block for the clock_round. As the threshold clock can advance many rounds at once (ex
        // because we synchronized a bulk of blocks) we can decide here whether we want to produce blocks per round
        // or just the latest one. From earlier experiments I saw only benefit on proposing for the penultimate round
        // only when the validator was supposed to be the leader of the round - so we bring down the missed leaders.
        // Probably proposing for all the intermediate rounds might not make much sense.

        // Consume the ancestors to be included in proposal
        let ancestors = self.ancestors_to_propose(clock_round, now);

        // Consume the next transactions to be included.
        let transactions = self.transaction_consumer.next();

        // Consume the commit votes to be included.
        let commit_votes = self
            .dag_state
            .write()
            .take_commit_votes(MAX_COMMIT_VOTES_PER_BLOCK);

        // Create the block and insert to storage.
        let block = Block::V1(BlockV1::new(
            self.context.committee.epoch(),
            clock_round,
            self.context.own_index,
            now,
            ancestors,
            transactions,
            commit_votes,
        ));
        let signed_block =
            SignedBlock::new(block, &self.block_signer).expect("Block signing failed.");
        let serialized = signed_block
            .serialize()
            .expect("Block serialization failed.");
        // Unnecessary to verify own blocks.
        let verified_block = VerifiedBlock::new_verified(signed_block, serialized);

        // Add to the threshold clock and pending ancestors
        self.threshold_clock.add_block(verified_block.reference());

        // Accept the block into BlockManager and DagState.
        let (accepted_blocks, missing) = self
            .block_manager
            .try_accept_blocks(vec![verified_block.clone()]);
        assert_eq!(accepted_blocks.len(), 1);
        assert!(missing.is_empty());

        // Ensure the new block and its ancestors are persisted, before broadcasting it.
        self.dag_state.write().flush();

        // Update internal state.
        self.last_proposed_block = verified_block.clone();

        tracing::info!("Created block {}", verified_block);

        Some(verified_block)
    }

    /// Runs commit rule to attempt to commit additional blocks from the DAG.
    fn try_commit(&mut self) -> ConsensusResult<Vec<CommittedSubDag>> {
        // TODO: Add optimization to abort early without quorum for a round.
        let sequenced_leaders = self.committer.try_commit(self.last_decided_leader);

        if let Some(last) = sequenced_leaders.last() {
            self.last_decided_leader = last.get_decided_slot();
            self.context
                .metrics
                .node_metrics
                .last_decided_leader_round
                .set(self.last_decided_leader.round as i64);
        }

        let committed_leaders = sequenced_leaders
            .into_iter()
            .filter_map(|leader| leader.into_committed_block())
            .collect::<Vec<_>>();

        self.commit_observer.handle_commit(committed_leaders)
    }

    pub(crate) fn get_missing_blocks(&self) -> BTreeSet<BlockRef> {
        self.block_manager.missing_blocks()
    }

    /// Retrieves the next ancestors to propose to form a block at `clock_round` round. Also, the `block_timestamp` is provided
    /// to sanity check that everything that goes into the proposal is ensured to have a timestamp < block_timestamp
    fn ancestors_to_propose(
        &mut self,
        clock_round: Round,
        block_timestamp: BlockTimestampMs,
    ) -> Vec<BlockRef> {
        // Now take the ancestors before the clock_round (excluded) for each authority.
        let ancestors = self
            .dag_state
            .read()
            .get_last_cached_block_per_authority(clock_round);
        assert_eq!(
            ancestors.len(),
            self.context.committee.size(),
            "Fatal error, number of returned ancestors don't match committee size."
        );

        // Propose only ancestors of higher rounds than what has already been proposed
        let ancestors = ancestors
            .into_iter()
            .flat_map(|block| {
                if let Some(last_block_ref) = self.last_included_ancestors[block.author()] {
                    return (last_block_ref.round < block.round()).then_some(block);
                }
                Some(block)
            })
            .collect::<Vec<_>>();

        // Update the last included ancestor block refs
        for ancestor in &ancestors {
            self.last_included_ancestors[ancestor.author()] = Some(ancestor.reference());
        }

        // TODO: this is for temporary sanity check - we might want to remove later on
        let mut quorum = StakeAggregator::<QuorumThreshold>::new();
        for ancestor in ancestors
            .iter()
            .filter(|block| block.round() == clock_round - 1)
        {
            quorum.add(ancestor.author(), &self.context.committee);
        }

        assert!(quorum.reached_threshold(&self.context.committee), "Fatal error, quorum not reached for parent round when proposing for round {}. Possible mismatch between DagState and Core.", clock_round);

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

        // Keep block refs to propose in a map, so even if somehow a byzantine node managed to provide blocks that don't
        // form a valid chain we can still pick one block per author.
        let mut to_propose = BTreeMap::new();
        for block in &ancestors {
            if !all_ancestors_parents.contains(&block.reference()) {
                to_propose.insert(block.author(), block.reference());
            }
        }

        assert!(!to_propose.is_empty());

        // always include our last proposed block in front of the vector and make sure that we do not
        // double insert.
        let mut result = vec![self.last_proposed_block.reference()];
        for (authority_index, block_ref) in to_propose {
            if authority_index != self.context.own_index {
                result.push(block_ref);
            }
        }

        result
    }

    /// Checks whether all the leaders of the previous quorum exist.
    /// TODO: we can leverage some additional signal here in order to more cleverly manipulate later the leader timeout
    /// Ex if we already have one leader - the first in order - we might don't want to wait as much.
    fn last_quorum_leaders_exist(&self) -> bool {
        let quorum_round = self.threshold_clock.get_round().saturating_sub(1);

        let dag_state = self.dag_state.read();
        for leader in self.leaders(quorum_round) {
            // Search for all the leaders. If at least one is not found, then return false.
            // A linear search should be fine here as the set of elements is not expected to be small enough and more sophisticated
            // data structures might not give us much here.
            if !dag_state.contains_cached_block_at_slot(leader) {
                return false;
            }
        }

        true
    }

    /// Returns the leaders of the provided round.
    fn leaders(&self, round: Round) -> Vec<Slot> {
        self.committer
            .get_leaders(round)
            .into_iter()
            .map(|authority_index| Slot::new(round, authority_index))
            .collect()
    }

    fn last_proposed_timestamp_ms(&self) -> BlockTimestampMs {
        self.last_proposed_block.timestamp_ms()
    }

    fn last_proposed_round(&self) -> Round {
        self.last_proposed_block.round()
    }

    #[cfg(test)]
    fn last_proposed_block(&self) -> &VerifiedBlock {
        &self.last_proposed_block
    }
}

/// Senders of signals from Core, for outputs and events (ex new block produced).
pub(crate) struct CoreSignals {
    tx_block_broadcast: broadcast::Sender<VerifiedBlock>,
    new_round_sender: watch::Sender<Round>,
}

impl CoreSignals {
    // TODO: move to Parameters.
    const BROADCAST_BACKLOG_CAPACITY: usize = 1000;

    pub fn new() -> (Self, CoreSignalsReceivers) {
        let (tx_block_broadcast, _rx_block_broadcast) =
            broadcast::channel::<VerifiedBlock>(Self::BROADCAST_BACKLOG_CAPACITY);
        let (new_round_sender, new_round_receiver) = watch::channel(0);

        let me = Self {
            tx_block_broadcast: tx_block_broadcast.clone(),
            new_round_sender,
        };

        let receivers = CoreSignalsReceivers {
            tx_block_broadcast,
            new_round_receiver,
        };

        (me, receivers)
    }

    /// Sends a signal to all the waiters that a new block has been produced. The method will return
    /// true if block has reached even one subscriber, false otherwise.
    pub fn new_block(&self, block: VerifiedBlock) -> ConsensusResult<()> {
        if let Err(err) = self.tx_block_broadcast.send(block) {
            warn!("Couldn't broadcast the block to any receiver: {err}");
            return Err(ConsensusError::Shutdown);
        }
        Ok(())
    }

    /// Sends a signal that threshold clock has advanced to new round. The `round_number` is the round at which the
    /// threshold clock has advanced to.
    pub fn new_round(&mut self, round_number: Round) {
        let _ = self.new_round_sender.send_replace(round_number);
    }
}

/// Receivers of signals from Core.
/// Intentially un-clonable. Comonents should only subscribe to channels they need.
pub(crate) struct CoreSignalsReceivers {
    tx_block_broadcast: broadcast::Sender<VerifiedBlock>,
    new_round_receiver: watch::Receiver<Round>,
}

impl CoreSignalsReceivers {
    pub(crate) fn block_broadcast_receiver(&self) -> broadcast::Receiver<VerifiedBlock> {
        self.tx_block_broadcast.subscribe()
    }

    pub(crate) fn new_round_receiver(&self) -> watch::Receiver<Round> {
        self.new_round_receiver.clone()
    }
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeSet, time::Duration};

    use consensus_config::{local_committee_and_keys, AuthorityIndex, Parameters, Stake};
    use sui_protocol_config::ProtocolConfig;
    use tokio::{
        sync::mpsc::{unbounded_channel, UnboundedReceiver},
        time::sleep,
    };

    use super::*;
    use crate::{
        block::genesis_blocks,
        block::TestBlock,
        block_verifier::NoopBlockVerifier,
        commit::CommitAPI as _,
        storage::{mem_store::MemStore, Store, WriteBatch},
        transaction::TransactionClient,
        CommitConsumer, CommitIndex,
    };

    /// Recover Core and continue proposing from the last round which forms a quorum.
    #[tokio::test]
    async fn test_core_recover_from_store_for_full_round() {
        telemetry_subscribers::init_for_testing();
        let (context, mut key_pairs) = Context::new_for_test(4);
        let context = Arc::new(context);
        let store = Arc::new(MemStore::new());
        let (_transaction_client, tx_receiver) = TransactionClient::new(context.clone());
        let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);

        // Create test blocks for all the authorities for 4 rounds and populate them in store
        let mut last_round_blocks = genesis_blocks(context.clone());
        let mut all_blocks: Vec<VerifiedBlock> = last_round_blocks.clone();
        for round in 1..=4 {
            let mut this_round_blocks = Vec::new();
            for (index, _authority) in context.committee.authorities() {
                let block = VerifiedBlock::new_for_test(
                    TestBlock::new(round, index.value() as u32)
                        .set_ancestors(last_round_blocks.iter().map(|b| b.reference()).collect())
                        .build(),
                );

                this_round_blocks.push(block);
            }
            all_blocks.extend(this_round_blocks.clone());
            last_round_blocks = this_round_blocks;
        }
        // write them in store
        store
            .write(WriteBatch::default().blocks(all_blocks))
            .expect("Storage error");

        // create dag state after all blocks have been written to store
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));
        let block_manager = BlockManager::new(
            context.clone(),
            dag_state.clone(),
            Arc::new(NoopBlockVerifier),
        );

        let (sender, _receiver) = unbounded_channel();
        let commit_observer = CommitObserver::new(
            context.clone(),
            CommitConsumer::new(sender.clone(), 0, 0),
            dag_state.clone(),
            store.clone(),
        );

        // Check no commits have been persisted to dag_state or store.
        let last_commit = store.read_last_commit().unwrap();
        assert!(last_commit.is_none());
        assert_eq!(dag_state.read().last_commit_index(), 0);

        // Now spin up core
        let (signals, signal_receivers) = CoreSignals::new();
        // Need at least one subscriber to the block broadcast channel.
        let mut block_receiver = signal_receivers.block_broadcast_receiver();
        let mut core = Core::new(
            context.clone(),
            transaction_consumer,
            block_manager,
            commit_observer,
            signals,
            key_pairs.remove(context.own_index.value()).1,
            dag_state.clone(),
        );

        // New round should be 5
        let mut new_round = signal_receivers.new_round_receiver();
        assert_eq!(*new_round.borrow_and_update(), 5);

        // Block for round 5 should have been proposed.
        let proposed_block = block_receiver
            .recv()
            .await
            .expect("A block should have been created");
        assert_eq!(proposed_block.round(), 5);
        let ancestors = proposed_block.ancestors();

        // Only ancestors of round 4 should be included.
        assert_eq!(ancestors.len(), 4);
        for ancestor in ancestors {
            assert_eq!(ancestor.round, 4);
        }

        // Run commit rule.
        core.try_commit().ok();
        let last_commit = store
            .read_last_commit()
            .unwrap()
            .expect("last commit should be set");

        // There were no commits prior to the core starting up but there was completed
        // rounds up to and including round 4. So we should commit leaders in round 1 & 2
        // as soon as the new block for round 5 is proposed.
        assert_eq!(last_commit.index(), 2);
        assert_eq!(dag_state.read().last_commit_index(), 2);
        let all_stored_commits = store.scan_commits(0..CommitIndex::MAX).unwrap();
        assert_eq!(all_stored_commits.len(), 2);
    }

    /// Recover Core and continue proposing when having a partial last round which doesn't form a quorum and we haven't
    /// proposed for that round yet.
    #[tokio::test]
    async fn test_core_recover_from_store_for_partial_round() {
        telemetry_subscribers::init_for_testing();

        let (context, mut key_pairs) = Context::new_for_test(4);
        let context = Arc::new(context);
        let store = Arc::new(MemStore::new());
        let (_transaction_client, tx_receiver) = TransactionClient::new(context.clone());
        let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);

        // Create test blocks for all authorities except our's (index = 0).
        let mut last_round_blocks = genesis_blocks(context.clone());
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
        store
            .write(WriteBatch::default().blocks(all_blocks))
            .expect("Storage error");

        // create dag state after all blocks have been written to store
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));
        let block_manager = BlockManager::new(
            context.clone(),
            dag_state.clone(),
            Arc::new(NoopBlockVerifier),
        );

        let (sender, _receiver) = unbounded_channel();
        let commit_observer = CommitObserver::new(
            context.clone(),
            CommitConsumer::new(sender.clone(), 0, 0),
            dag_state.clone(),
            store.clone(),
        );

        // Check no commits have been persisted to dag_state & store
        let last_commit = store.read_last_commit().unwrap();
        assert!(last_commit.is_none());
        assert_eq!(dag_state.read().last_commit_index(), 0);

        // Now spin up core
        let (signals, signal_receivers) = CoreSignals::new();
        // Need at least one subscriber to the block broadcast channel.
        let mut block_receiver = signal_receivers.block_broadcast_receiver();
        let mut core = Core::new(
            context.clone(),
            transaction_consumer,
            block_manager,
            commit_observer,
            signals,
            key_pairs.remove(context.own_index.value()).1,
            dag_state.clone(),
        );

        // New round should be 4
        let mut new_round = signal_receivers.new_round_receiver();
        assert_eq!(*new_round.borrow_and_update(), 4);

        // When trying to propose now we should propose block for round 4
        let proposed_block = block_receiver
            .recv()
            .await
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

        // Run commit rule.
        core.try_commit().ok();
        let last_commit = store
            .read_last_commit()
            .unwrap()
            .expect("last commit should be set");

        // There were no commits prior to the core starting up but there was completed
        // rounds up to round 4. So we should commit leaders in round 1 & 2 as soon
        // as the new block for round 4 is proposed.
        assert_eq!(last_commit.index(), 2);
        assert_eq!(dag_state.read().last_commit_index(), 2);
        let all_stored_commits = store.scan_commits(0..CommitIndex::MAX).unwrap();
        assert_eq!(all_stored_commits.len(), 2);
    }

    #[tokio::test]
    async fn test_core_propose_after_genesis() {
        telemetry_subscribers::init_for_testing();
        let _guard = ProtocolConfig::apply_overrides_for_testing(|_, mut config| {
            config.set_consensus_max_transaction_size_bytes(2_000);
            config.set_consensus_max_transactions_in_block_bytes(2_000);
            config
        });

        let (context, mut key_pairs) = Context::new_for_test(4);
        let context = Arc::new(context);
        let store = Arc::new(MemStore::new());
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));

        let block_manager = BlockManager::new(
            context.clone(),
            dag_state.clone(),
            Arc::new(NoopBlockVerifier),
        );
        let (transaction_client, tx_receiver) = TransactionClient::new(context.clone());
        let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);
        let (signals, signal_receivers) = CoreSignals::new();
        // Need at least one subscriber to the block broadcast channel.
        let mut block_receiver = signal_receivers.block_broadcast_receiver();

        let (sender, _receiver) = unbounded_channel();
        let commit_observer = CommitObserver::new(
            context.clone(),
            CommitConsumer::new(sender.clone(), 0, 0),
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
            dag_state.clone(),
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

        // a new block should have been created during recovery.
        let block = block_receiver
            .recv()
            .await
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
        let all_genesis = genesis_blocks(context);

        for ancestor in block.ancestors() {
            all_genesis
                .iter()
                .find(|block| block.reference() == *ancestor)
                .expect("Block should be found amongst genesis blocks");
        }

        // Try to propose again - with or without ignore leaders check, it will not return any block
        assert!(core.try_propose(false).unwrap().is_none());
        assert!(core.try_propose(true).unwrap().is_none());

        // Check no commits have been persisted to dag_state & store
        let last_commit = store.read_last_commit().unwrap();
        assert!(last_commit.is_none());
        assert_eq!(dag_state.read().last_commit_index(), 0);
    }

    #[tokio::test]
    async fn test_core_propose_once_receiving_a_quorum() {
        telemetry_subscribers::init_for_testing();
        let (context, mut key_pairs) = Context::new_for_test(4);
        let context = Arc::new(context);

        let store = Arc::new(MemStore::new());
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));

        let block_manager = BlockManager::new(
            context.clone(),
            dag_state.clone(),
            Arc::new(NoopBlockVerifier),
        );
        let (_transaction_client, tx_receiver) = TransactionClient::new(context.clone());
        let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);
        let (signals, signal_receivers) = CoreSignals::new();
        // Need at least one subscriber to the block broadcast channel.
        let _block_receiver = signal_receivers.block_broadcast_receiver();

        let (sender, _receiver) = unbounded_channel();
        let commit_observer = CommitObserver::new(
            context.clone(),
            CommitConsumer::new(sender.clone(), 0, 0),
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
            dag_state.clone(),
        );

        let mut expected_ancestors = BTreeSet::new();

        // Adding one block now will trigger the creation of new block for round 1
        let block_1 = VerifiedBlock::new_for_test(TestBlock::new(1, 1).build());
        expected_ancestors.insert(block_1.reference());
        // Wait for min round delay to allow blocks to be proposed.
        sleep(context.parameters.min_round_delay).await;
        // add blocks to trigger proposal.
        _ = core.add_blocks(vec![block_1]);

        assert_eq!(core.last_proposed_round(), 1);
        expected_ancestors.insert(core.last_proposed_block().reference());
        // attempt to create a block - none will be produced.
        assert!(core.try_propose(false).unwrap().is_none());

        // Adding another block now forms a quorum for round 1, so block at round 2 will proposed
        let block_3 = VerifiedBlock::new_for_test(TestBlock::new(1, 2).build());
        expected_ancestors.insert(block_3.reference());
        // Wait for min round delay to allow blocks to be proposed.
        sleep(context.parameters.min_round_delay).await;
        // add blocks to trigger proposal.
        _ = core.add_blocks(vec![block_3]);

        assert_eq!(core.last_proposed_round(), 2);

        let proposed_block = core.last_proposed_block();
        assert_eq!(proposed_block.round(), 2);
        assert_eq!(proposed_block.author(), context.own_index);
        assert_eq!(proposed_block.ancestors().len(), 3);
        let ancestors = proposed_block.ancestors();
        let ancestors = ancestors.iter().cloned().collect::<BTreeSet<_>>();
        assert_eq!(ancestors, expected_ancestors);

        // Check no commits have been persisted to dag_state & store
        let last_commit = store.read_last_commit().unwrap();
        assert!(last_commit.is_none());
        assert_eq!(dag_state.read().last_commit_index(), 0);
    }

    #[tokio::test]
    async fn test_core_try_new_block_leader_timeout() {
        telemetry_subscribers::init_for_testing();

        // Create the cores for all authorities
        let mut all_cores = create_cores(vec![1, 1, 1, 1]);

        // Create blocks for rounds 1..=3 from all Cores except last Core of authority 3, so we miss the block from it. As
        // it will be the leader of round 3 then no-one will be able to progress to round 4 unless we explicitly trigger
        // the block creation.
        // create the cores and their signals for all the authorities
        let (_last_core, cores) = all_cores.split_last_mut().unwrap();

        // Now iterate over a few rounds and ensure the corresponding signals are created while network advances
        let mut last_round_blocks = Vec::new();
        for round in 1..=3 {
            let mut this_round_blocks = Vec::new();

            for (core, _signal_receivers, _, _, _) in cores.iter_mut() {
                core.add_blocks(last_round_blocks.clone()).unwrap();

                // Only when round > 1 and using non-genesis parents.
                if let Some(r) = last_round_blocks.first().map(|b| b.round()) {
                    assert_eq!(round - 1, r);
                    // Ensure no new block is proposed before the min round delay.
                    assert_eq!(core.last_proposed_round(), r);
                    // Force propose new block regardless of min round delay.
                    core.try_propose(true).unwrap().unwrap_or_else(|| {
                        panic!("Block should have been proposed for round {}", round)
                    });
                }

                assert_eq!(core.last_proposed_round(), round);

                this_round_blocks.push(core.last_proposed_block.clone());
            }

            last_round_blocks = this_round_blocks;
        }

        // Try to create the blocks for round 4 by calling the try_propose() method. No block should be created as the
        // leader - authority 3 - hasn't proposed any block.
        for (core, _, _, _, _) in cores.iter_mut() {
            core.add_blocks(last_round_blocks.clone()).unwrap();
            assert!(core.try_propose(false).unwrap().is_none());
        }

        // Now try to create the blocks for round 4 via the leader timeout method which should
        // ignore any leader checks or min round delay.
        for (core, _, _, _, store) in cores.iter_mut() {
            assert!(core.force_new_block(4).unwrap().is_some());
            assert_eq!(core.last_proposed_round(), 4);

            // Check commits have been persisted to store
            let last_commit = store
                .read_last_commit()
                .unwrap()
                .expect("last commit should be set");
            // There are 1 leader rounds with rounds completed up to and including
            // round 4
            assert_eq!(last_commit.index(), 1);
            let all_stored_commits = store.scan_commits(0..CommitIndex::MAX).unwrap();
            assert_eq!(all_stored_commits.len(), 1);
        }
    }

    #[tokio::test]
    async fn test_core_signals() {
        telemetry_subscribers::init_for_testing();
        let default_params = Parameters::default();

        // create the cores and their signals for all the authorities
        let mut cores = create_cores(vec![1, 1, 1, 1]);

        // Now iterate over a few rounds and ensure the corresponding signals are created while network advances
        let mut last_round_blocks = Vec::new();
        for round in 1..=10 {
            let mut this_round_blocks = Vec::new();

            for (core, signal_receivers, block_receiver, _, _) in &mut cores {
                // Wait for min round delay to allow blocks to be proposed.
                sleep(default_params.min_round_delay).await;
                // add the blocks from last round
                // this will trigger a block creation for the round and a signal should be emitted
                core.add_blocks(last_round_blocks.clone()).unwrap();

                // A "new round" signal should be received given that all the blocks of previous round have been processed
                let new_round = receive(
                    Duration::from_secs(1),
                    signal_receivers.new_round_receiver(),
                )
                .await;
                assert_eq!(new_round, round);

                // Check that a new block has been proposed.
                let block = tokio::time::timeout(Duration::from_secs(1), block_receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(block.round(), round);
                assert_eq!(block.author(), core.context.own_index);

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

        for (_, _, _, _, store) in cores {
            // Check commits have been persisted to store
            let last_commit = store
                .read_last_commit()
                .unwrap()
                .expect("last commit should be set");
            // There are 8 leader rounds with rounds completed up to and including
            // round 9. Round 10 blocks will only include their own blocks, so the
            // 8th leader will not be committed.
            assert_eq!(last_commit.index(), 7);
            let all_stored_commits = store.scan_commits(0..CommitIndex::MAX).unwrap();
            assert_eq!(all_stored_commits.len(), 7);
        }
    }

    #[tokio::test]
    async fn test_core_compress_proposal_references() {
        telemetry_subscribers::init_for_testing();
        let default_params = Parameters::default();

        // create the cores and their signals for all the authorities
        let mut cores = create_cores(vec![1, 1, 1, 1]);

        let mut last_round_blocks = Vec::new();
        let mut all_blocks = Vec::new();

        let excluded_authority = AuthorityIndex::new_for_test(3);

        for round in 1..=10 {
            let mut this_round_blocks = Vec::new();

            for (core, _, _, _, _) in &mut cores {
                // do not produce any block for authority 3
                if core.context.own_index == excluded_authority {
                    continue;
                }

                // try to propose to ensure that we are covering the case where we miss the leader authority 3
                core.add_blocks(last_round_blocks.clone()).unwrap();
                core.force_new_block(round).unwrap();

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
        let (core, _, _, _, store) = &mut cores[excluded_authority];
        // Wait for min round delay to allow blocks to be proposed.
        sleep(default_params.min_round_delay).await;
        // add blocks to trigger proposal.
        core.add_blocks(all_blocks).unwrap();

        // Assert that a block has been created for round 11 and it references to blocks of round 10 for the other peers, and
        // to round 1 for its own block (created after recovery).
        let block = core.last_proposed_block();
        assert_eq!(block.round(), 11);
        assert_eq!(block.ancestors().len(), 4);
        for block_ref in block.ancestors() {
            if block_ref.author == excluded_authority {
                assert_eq!(block_ref.round, 1);
            } else {
                assert_eq!(block_ref.round, 10);
            }
        }

        // Check commits have been persisted to store
        let last_commit = store
            .read_last_commit()
            .unwrap()
            .expect("last commit should be set");
        // There are 8 leader rounds with rounds completed up to and including
        // round 10. However because there were no blocks produced for authority 3
        // 2 leader rounds will be skipped.
        assert_eq!(last_commit.index(), 6);
        let all_stored_commits = store.scan_commits(0..CommitIndex::MAX).unwrap();
        assert_eq!(all_stored_commits.len(), 6);
    }

    /// Creates cores for the specified number of authorities for their corresponding stakes. The method returns the
    /// cores and their respective signal receivers are returned in `AuthorityIndex` order asc.
    // TODO: return a test fixture instead.
    fn create_cores(
        authorities: Vec<Stake>,
    ) -> Vec<(
        Core,
        CoreSignalsReceivers,
        broadcast::Receiver<VerifiedBlock>,
        UnboundedReceiver<CommittedSubDag>,
        Arc<impl Store>,
    )> {
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

            let block_manager = BlockManager::new(
                context.clone(),
                dag_state.clone(),
                Arc::new(NoopBlockVerifier),
            );
            let (_transaction_client, tx_receiver) = TransactionClient::new(context.clone());
            let transaction_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);
            let (signals, signal_receivers) = CoreSignals::new();
            // Need at least one subscriber to the block broadcast channel.
            let block_receiver = signal_receivers.block_broadcast_receiver();

            let (commit_sender, commit_receiver) = unbounded_channel();
            let commit_observer = CommitObserver::new(
                context.clone(),
                CommitConsumer::new(commit_sender.clone(), 0, 0),
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
                dag_state,
            );

            cores.push((
                core,
                signal_receivers,
                block_receiver,
                commit_receiver,
                store,
            ));
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
