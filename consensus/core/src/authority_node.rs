// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{sync::Arc, time::Duration, time::Instant, vec};

use async_trait::async_trait;
use bytes::Bytes;
use consensus_config::{AuthorityIndex, Committee, NetworkKeyPair, Parameters, ProtocolKeyPair};
use parking_lot::RwLock;
use prometheus::Registry;
use sui_protocol_config::ProtocolConfig;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{
    block::{timestamp_utc_ms, BlockAPI, BlockRef, SignedBlock, VerifiedBlock},
    block_manager::BlockManager,
    block_verifier::{BlockVerifier, SignedBlockVerifier},
    broadcaster::Broadcaster,
    commit_observer::CommitObserver,
    context::Context,
    core::{Core, CoreSignals},
    core_thread::{ChannelCoreThreadDispatcher, CoreThreadDispatcher, CoreThreadHandle},
    dag_state::DagState,
    error::{ConsensusError, ConsensusResult},
    leader_timeout::{LeaderTimeoutTask, LeaderTimeoutTaskHandle},
    metrics::initialise_metrics,
    network::{anemo_network::AnemoManager, NetworkManager, NetworkService},
    storage::rocksdb_store::RocksDBStore,
    synchronizer::{Synchronizer, SynchronizerHandle},
    transaction::{TransactionClient, TransactionConsumer, TransactionVerifier},
    CommitConsumer,
};

// This type is used by Sui as part of starting consensus via MysticetiManager.
// It hides the details of the types.
pub struct ConsensusAuthority(AuthorityNode<AnemoManager>);

impl ConsensusAuthority {
    pub async fn start(
        own_index: AuthorityIndex,
        committee: Committee,
        parameters: Parameters,
        protocol_config: ProtocolConfig,
        protocol_keypair: ProtocolKeyPair,
        network_keypair: NetworkKeyPair,
        transaction_verifier: Arc<dyn TransactionVerifier>,
        commit_consumer: CommitConsumer,
        registry: Registry,
    ) -> Self {
        let authority_node = AuthorityNode::start(
            own_index,
            committee,
            parameters,
            protocol_config,
            protocol_keypair,
            network_keypair,
            transaction_verifier,
            commit_consumer,
            registry,
        )
        .await;
        Self(authority_node)
    }

    pub async fn stop(self) {
        self.0.stop().await;
    }

    pub fn transaction_client(&self) -> Arc<TransactionClient> {
        self.0.transaction_client()
    }
}

pub(crate) struct AuthorityNode<N>
where
    N: NetworkManager<AuthorityService<ChannelCoreThreadDispatcher>>,
{
    context: Arc<Context>,
    start_time: Instant,
    transaction_client: Arc<TransactionClient>,
    leader_timeout_handle: LeaderTimeoutTaskHandle,
    core_thread_handle: CoreThreadHandle,
    broadcaster: Broadcaster,
    network_manager: N,
    synchronizer: Arc<SynchronizerHandle>,
}

impl<N> AuthorityNode<N>
where
    N: NetworkManager<AuthorityService<ChannelCoreThreadDispatcher>>,
{
    pub(crate) async fn start(
        own_index: AuthorityIndex,
        committee: Committee,
        parameters: Parameters,
        protocol_config: ProtocolConfig,
        // To avoid accidentally leaking the private key, the protocol key pair should only be
        // kept in Core.
        protocol_keypair: ProtocolKeyPair,
        network_keypair: NetworkKeyPair,
        transaction_verifier: Arc<dyn TransactionVerifier>,
        commit_consumer: CommitConsumer,
        registry: Registry,
    ) -> Self {
        info!("Starting authority with index {}", own_index);
        let context = Arc::new(Context::new(
            own_index,
            committee,
            parameters,
            protocol_config,
            initialise_metrics(registry),
        ));
        let start_time = Instant::now();

        // Create the transactions client and the transactions consumer
        let (tx_client, tx_receiver) = TransactionClient::new(context.clone());
        let tx_consumer = TransactionConsumer::new(tx_receiver, context.clone(), None);

        // Construct Core components.
        let (core_signals, signals_receivers) = CoreSignals::new();
        let store = Arc::new(RocksDBStore::new(&context.parameters.db_path_str_unsafe()));
        let dag_state = Arc::new(RwLock::new(DagState::new(context.clone(), store.clone())));
        let block_manager = BlockManager::new(context.clone(), dag_state.clone());
        let commit_observer = CommitObserver::new(
            context.clone(),
            commit_consumer.sender,
            commit_consumer.last_processed_index,
            dag_state,
            store.clone(),
        );
        let core = Core::new(
            context.clone(),
            tx_consumer,
            block_manager,
            commit_observer,
            core_signals,
            protocol_keypair,
            store,
        );

        let (core_dispatcher, core_thread_handle) =
            ChannelCoreThreadDispatcher::start(core, context.clone());
        let core_dispatcher = Arc::new(core_dispatcher);
        let leader_timeout_handle =
            LeaderTimeoutTask::start(core_dispatcher.clone(), &signals_receivers, context.clone());

        // Create network manager and client.
        let network_manager = N::new(context.clone());
        let network_client = network_manager.client();

        // Create Broadcaster.
        let broadcaster = Broadcaster::new(context.clone(), network_client.clone(), &signals_receivers);

        // Start network service.
        let block_verifier = Arc::new(SignedBlockVerifier::new(
            context.clone(),
            transaction_verifier,
        ));
        let synchronizer = Synchronizer::start(
            network_client,
            context.clone(),
            core_dispatcher.clone(),
            block_verifier.clone(),
        );
        let network_service = Arc::new(AuthorityService {
            context: context.clone(),
            block_verifier,
            core_dispatcher,
            synchronizer: synchronizer.clone(),
        });
        network_manager.install_service(network_keypair, network_service);

        Self {
            context,
            start_time,
            transaction_client: Arc::new(tx_client),
            leader_timeout_handle,
            core_thread_handle,
            broadcaster,
            network_manager,
            synchronizer,
        }
    }

    pub(crate) async fn stop(mut self) {
        info!(
            "Stopping authority. Total run time: {:?}",
            self.start_time.elapsed()
        );

        self.network_manager.stop().await;
        self.broadcaster.stop();
        self.core_thread_handle.stop();
        self.leader_timeout_handle.stop().await;
        self.synchronizer.stop().await;

        self.context
            .metrics
            .node_metrics
            .uptime
            .observe(self.start_time.elapsed().as_secs_f64());
    }

    pub(crate) fn transaction_client(&self) -> Arc<TransactionClient> {
        self.transaction_client.clone()
    }
}

/// Authority's network interface.
pub(crate) struct AuthorityService<C: CoreThreadDispatcher> {
    context: Arc<Context>,
    block_verifier: Arc<dyn BlockVerifier>,
    core_dispatcher: C,
    synchronizer: Arc<SynchronizerHandle>,
}

#[async_trait]
impl<C: CoreThreadDispatcher> NetworkService for AuthorityService<C> {
    async fn handle_send_block(
        &self,
        peer: AuthorityIndex,
        serialized_block: Bytes,
    ) -> ConsensusResult<()> {
        // TODO: dedup block verifications, here and with fetched blocks.
        let signed_block: SignedBlock =
            bcs::from_bytes(&serialized_block).map_err(ConsensusError::MalformedBlock)?;

        // Reject blocks not produced by the peer.
        if peer != signed_block.author() {
            self.context
                .metrics
                .node_metrics
                .invalid_blocks
                .with_label_values(&[&peer.to_string()])
                .inc();
            let e = ConsensusError::UnexpectedAuthority(signed_block.author(), peer);
            info!("Block with wrong authority from {}: {}", peer, e);
            return Err(e);
        }

        // Reject blocks failing validations.
        if let Err(e) = self.block_verifier.verify(&signed_block) {
            self.context
                .metrics
                .node_metrics
                .invalid_blocks
                .with_label_values(&[&peer.to_string()])
                .inc();
            info!("Invalid block from {}: {}", peer, e);
            return Err(e);
        }
        let verified_block = VerifiedBlock::new_verified(signed_block, serialized_block);

        // Reject block with timestamp too far in the future.
        let forward_time_drift = Duration::from_millis(
            verified_block
                .timestamp_ms()
                .saturating_sub(timestamp_utc_ms()),
        );
        if forward_time_drift > self.context.parameters.max_forward_time_drift {
            return Err(ConsensusError::BlockTooFarInFuture {
                block_timestamp: verified_block.timestamp_ms(),
                forward_time_drift,
            });
        }

        // Wait until the block's timestamp is current.
        if forward_time_drift > Duration::ZERO {
            self.context
                .metrics
                .node_metrics
                .block_timestamp_drift_wait_ms
                .with_label_values(&[&peer.to_string()])
                .inc_by(forward_time_drift.as_millis() as u64);
            sleep(forward_time_drift).await;
        }

        let missing_ancestors = self
            .core_dispatcher
            .add_blocks(vec![verified_block])
            .await
            .map_err(|_| ConsensusError::Shutdown)?;

        if !missing_ancestors.is_empty() {
            // schedule the fetching of them from this peer
            if let Err(err) = self
                .synchronizer
                .fetch_blocks(missing_ancestors, peer)
                .await
            {
                warn!("Errored while trying to fetch missing ancestors via synchronizer: {err}");
            }
        }

        Ok(())
    }

    async fn handle_fetch_blocks(
        &self,
        _peer: AuthorityIndex,
        _block_refs: Vec<BlockRef>,
    ) -> ConsensusResult<Vec<Bytes>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use async_trait::async_trait;
    use consensus_config::{local_committee_and_keys, NetworkKeyPair, Parameters, ProtocolKeyPair};
    use fastcrypto::traits::ToFromBytes;
    use parking_lot::Mutex;
    use prometheus::Registry;
    use sui_protocol_config::ProtocolConfig;
    use tempfile::TempDir;
    use tokio::sync::mpsc::unbounded_channel;
    use tokio::time::sleep;

    use super::*;
    use crate::authority_node::AuthorityService;
    use crate::block::{timestamp_utc_ms, BlockRef, Round, TestBlock, VerifiedBlock};
    use crate::block_verifier::NoopBlockVerifier;
    use crate::context::Context;
    use crate::core_thread::{CoreError, CoreThreadDispatcher};
    use crate::network::NetworkService;
    use crate::transaction::NoopTransactionVerifier;

    struct FakeCoreThreadDispatcher {
        blocks: Mutex<Vec<VerifiedBlock>>,
    }

    impl FakeCoreThreadDispatcher {
        fn new() -> Self {
            Self {
                blocks: Mutex::new(vec![]),
            }
        }

        fn get_blocks(&self) -> Vec<VerifiedBlock> {
            self.blocks.lock().clone()
        }
    }

    #[async_trait]
    impl CoreThreadDispatcher for Arc<FakeCoreThreadDispatcher> {
        async fn add_blocks(&self, blocks: Vec<VerifiedBlock>) -> Result<Vec<BlockRef>, CoreError> {
            let block_refs = blocks.iter().map(|b| b.reference()).collect();
            self.blocks.lock().extend(blocks);
            Ok(block_refs)
        }

        async fn force_new_block(&self, _round: Round) -> Result<(), CoreError> {
            unimplemented!()
        }

        async fn get_missing_blocks(&self) -> Result<Vec<BTreeSet<BlockRef>>, CoreError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn start_and_stop() {
        let (committee, keypairs) = local_committee_and_keys(0, vec![1]);
        let registry = Registry::new();

        let temp_dir = TempDir::new().unwrap();
        let parameters = Parameters {
            db_path: Some(temp_dir.into_path()),
            ..Default::default()
        };
        let txn_verifier = NoopTransactionVerifier {};

        let (own_index, _) = committee.authorities().last().unwrap();
        let protocol_keypair = ProtocolKeyPair::from_bytes(keypairs[0].1.as_bytes()).unwrap();
        let network_keypair = NetworkKeyPair::from_bytes(keypairs[0].0.as_bytes()).unwrap();

        let (sender, _receiver) = unbounded_channel();
        let commit_consumer = CommitConsumer::new(
            sender, 0, // last_processed_index
        );

        let authority = ConsensusAuthority::start(
            own_index,
            committee,
            parameters,
            ProtocolConfig::get_for_max_version_UNSAFE(),
            protocol_keypair,
            network_keypair,
            Arc::new(txn_verifier),
            commit_consumer,
            registry,
        )
        .await;

        assert_eq!(authority.0.context.own_index, own_index);
        assert_eq!(authority.0.context.committee.epoch(), 0);
        assert_eq!(authority.0.context.committee.size(), 1);

        authority.stop().await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_authority_service() {
        let (context, _keys) = Context::new_for_test(4);
        let context = Arc::new(context);
        let block_verifier = NoopBlockVerifier {};
        let core_dispatcher = Arc::new(FakeCoreThreadDispatcher::new());
        let network_manager = AnemoManager::new(context.clone());
        let network_client = network_manager.client();
        let synchronizer = Synchronizer::start(network_client, context.clone(), core_dispatcher.clone());
        let authority_service = Arc::new(AuthorityService {
            context: context.clone(),
            block_verifier: Arc::new(block_verifier),
            core_dispatcher: core_dispatcher.clone(),
            synchronizer
        });

        // Test delaying blocks with time drift.
        let now = timestamp_utc_ms();
        let max_drift = context.parameters.max_forward_time_drift;
        let input_block = VerifiedBlock::new_for_test(
            TestBlock::new(9, 0)
                .set_timestamp_ms(now + max_drift.as_millis() as u64)
                .build(),
        );

        let service = authority_service.clone();
        let serialized = input_block.serialized().clone();
        tokio::spawn(async move {
            service
                .handle_send_block(context.committee.to_authority_index(0).unwrap(), serialized)
                .await
                .unwrap();
        });

        sleep(max_drift / 2).await;
        assert!(core_dispatcher.get_blocks().is_empty());

        sleep(max_drift).await;
        let blocks = core_dispatcher.get_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], input_block);
    }
}
