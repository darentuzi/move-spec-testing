// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    consensus_observer::{
        error::Error,
        logging::{LogEntry, LogSchema},
        metrics,
        network_client::ConsensusObserverClient,
        network_events::{ConsensusObserverNetworkEvents, NetworkMessage, ResponseSender},
        network_message::{
            BlockPayload, CommitDecision, ConsensusObserverDirectSend, ConsensusObserverMessage,
            ConsensusObserverRequest, ConsensusObserverResponse, OrderedBlock,
        },
        payload_store::BlockPayloadStore,
        pending_blocks::PendingOrderedBlocks,
        publisher::ConsensusPublisher,
        subscription,
        subscription::ConsensusObserverSubscription,
    },
    dag::DagCommitSigner,
    network::{IncomingCommitRequest, IncomingRandGenRequest},
    network_interface::CommitMessage,
    payload_manager::PayloadManager,
    pipeline::execution_client::TExecutionClient,
    state_replication::StateComputerCommitCallBackType,
};
use aptos_channels::{aptos_channel, message_queues::QueueStyle};
use aptos_config::{config::ConsensusObserverConfig, network_id::PeerNetworkId};
use aptos_consensus_types::pipeline;
use aptos_crypto::{bls12381, Genesis};
use aptos_event_notifications::{DbBackedOnChainConfig, ReconfigNotificationListener};
use aptos_infallible::Mutex;
use aptos_logger::{debug, error, info, warn};
use aptos_network::{
    application::{interface::NetworkClient, metadata::PeerMetadata},
    protocols::wire::handshake::v1::ProtocolId,
};
use aptos_reliable_broadcast::DropGuard;
use aptos_storage_interface::DbReader;
use aptos_time_service::TimeService;
use aptos_types::{
    block_info::{BlockInfo, Round},
    epoch_state::EpochState,
    ledger_info::LedgerInfoWithSignatures,
    on_chain_config::{
        OnChainConsensusConfig, OnChainExecutionConfig, OnChainRandomnessConfig,
        RandomnessConfigMoveStruct, ValidatorSet,
    },
    validator_signer::ValidatorSigner,
};
use futures::{
    future::{AbortHandle, Abortable},
    StreamExt,
};
use futures_channel::oneshot;
use move_core_types::account_address::AccountAddress;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::mpsc::UnboundedSender, time::interval};
use tokio_stream::wrappers::IntervalStream;

/// The consensus observer receives consensus updates and propagates them to the execution pipeline
pub struct ConsensusObserver {
    // The configuration of the consensus observer
    consensus_observer_config: ConsensusObserverConfig,
    // The consensus observer client to send network messages
    consensus_observer_client:
        Arc<ConsensusObserverClient<NetworkClient<ConsensusObserverMessage>>>,

    // The current epoch state
    epoch_state: Option<Arc<EpochState>>,
    // The latest ledger info (updated via a callback)
    root: Arc<Mutex<LedgerInfoWithSignatures>>,

    // The payload store holds block transaction payloads
    block_payload_store: BlockPayloadStore,
    // The pending ordered blocks (these are also buffered when in state sync mode)
    pending_ordered_blocks: PendingOrderedBlocks,
    // The execution client to the buffer manager
    execution_client: Arc<dyn TExecutionClient>,

    // If the sync handle is set it indicates that we're in state sync mode
    sync_handle: Option<DropGuard>,
    // The sender to notify the consensus observer that state sync to the (epoch, round) is done
    sync_notification_sender: UnboundedSender<(u64, Round)>,
    // The reconfiguration event listener to refresh on-chain configs
    reconfig_events: Option<ReconfigNotificationListener<DbBackedOnChainConfig>>,

    // The consensus publisher to forward payload messages
    consensus_publisher: Option<Arc<ConsensusPublisher>>,
    // The currently active consensus observer subscription
    active_observer_subscription: Option<ConsensusObserverSubscription>,
    // A handle to storage (used to read the latest state and check progress)
    db_reader: Arc<dyn DbReader>,
    // The time service (used to check progress)
    time_service: TimeService,
}

impl ConsensusObserver {
    pub fn new(
        consensus_observer_config: ConsensusObserverConfig,
        consensus_observer_client: Arc<
            ConsensusObserverClient<NetworkClient<ConsensusObserverMessage>>,
        >,
        db_reader: Arc<dyn DbReader>,
        execution_client: Arc<dyn TExecutionClient>,
        sync_notification_sender: UnboundedSender<(u64, Round)>,
        reconfig_events: Option<ReconfigNotificationListener<DbBackedOnChainConfig>>,
        consensus_publisher: Option<Arc<ConsensusPublisher>>,
        time_service: TimeService,
    ) -> Self {
        // Read the latest ledger info from storage
        let root = db_reader
            .get_latest_ledger_info()
            .expect("Failed to read latest ledger info!");

        Self {
            consensus_observer_config,
            consensus_observer_client,
            epoch_state: None,
            root: Arc::new(Mutex::new(root)),
            pending_ordered_blocks: PendingOrderedBlocks::new(consensus_observer_config),
            execution_client,
            block_payload_store: BlockPayloadStore::new(),
            sync_handle: None,
            sync_notification_sender,
            reconfig_events,
            consensus_publisher,
            active_observer_subscription: None,
            db_reader,
            time_service,
        }
    }

    /// Checks the progress of the consensus observer
    async fn check_progress(&mut self) {
        debug!(LogSchema::new(LogEntry::ConsensusObserver)
            .message("Checking consensus observer progress!"));

        // Get the peer ID of the currently active subscription (if any)
        let active_subscription_peer = self
            .active_observer_subscription
            .as_ref()
            .map(|subscription| subscription.get_peer_network_id());

        // If we have an active subscription, verify that the subscription
        // is still healthy. If not, the subscription should be terminated.
        if let Some(active_subscription_peer) = active_subscription_peer {
            if let Err(error) = self.check_active_subscription() {
                // Log the subscription termination
                warn!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Terminating subscription to peer: {:?}! Error: {:?}",
                        active_subscription_peer, error
                    ))
                );

                // Unsubscribe from the peer
                self.unsubscribe_from_peer(active_subscription_peer);

                // Update the subscription termination metrics
                self.update_subscription_termination_metrics(active_subscription_peer, error);
            }
        }

        // If we don't have a subscription, we should select a new peer to
        // subscribe to. If we had a previous subscription, it should be
        // excluded from the selection process.
        if self.active_observer_subscription.is_none() {
            // Create a new observer subscription
            self.create_new_observer_subscription(active_subscription_peer)
                .await;

            // If we successfully created a new subscription, update the subscription creation metrics
            if let Some(active_subscription) = &self.active_observer_subscription {
                self.update_subscription_creation_metrics(
                    active_subscription.get_peer_network_id(),
                );
            }
        }
    }

    /// Checks if the active subscription is still healthy. If not, an error is returned.
    fn check_active_subscription(&mut self) -> Result<(), Error> {
        let active_observer_subscription = self.active_observer_subscription.take();
        if let Some(mut active_subscription) = active_observer_subscription {
            // Check if the peer for the subscription is still connected
            let peer_network_id = active_subscription.get_peer_network_id();
            let peer_still_connected = self
                .get_connected_peers_and_metadata()
                .map_or(false, |peers_and_metadata| {
                    peers_and_metadata.contains_key(&peer_network_id)
                });

            // Verify the peer is still connected
            if !peer_still_connected {
                return Err(Error::SubscriptionDisconnected(
                    "The peer is no longer connected!".to_string(),
                ));
            }

            // Verify the subscription has not timed out
            active_subscription.check_subscription_timeout()?;

            // Verify that the DB is continuing to sync and commit new data.
            // Note: we should only do this if we're not waiting for state sync.
            active_subscription.check_syncing_progress()?;

            // Verify that the subscription peer is optimal
            if let Some(peers_and_metadata) = self.get_connected_peers_and_metadata() {
                active_subscription.check_subscription_peer_optimality(peers_and_metadata)?;
            }

            // The subscription seems healthy, we can keep it
            self.active_observer_subscription = Some(active_subscription);
        }

        Ok(())
    }

    /// Creates and returns a commit callback (to be called after the execution pipeline)
    fn create_commit_callback(&self) -> StateComputerCommitCallBackType {
        // Clone the root, pending blocks and payload store
        let root = self.root.clone();
        let pending_ordered_blocks = self.pending_ordered_blocks.clone();
        let block_payload_store = self.block_payload_store.clone();

        // Create the commit callback
        Box::new(move |blocks, ledger_info: LedgerInfoWithSignatures| {
            // Remove the committed blocks from the payload store
            block_payload_store.remove_blocks(blocks);

            // Remove the committed blocks from the pending blocks
            pending_ordered_blocks.remove_blocks_for_commit(&ledger_info);

            // Verify the ledger info is for the same epoch
            let mut root = root.lock();
            if ledger_info.commit_info().epoch() != root.commit_info().epoch() {
                warn!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Received commit callback for a different epoch! Ledger info: {:?}, Root: {:?}",
                        ledger_info.commit_info(),
                        root.commit_info()
                    ))
                );
                return;
            }

            // Update the root ledger info. Note: we only want to do this if
            // the new ledger info round is greater than the current root
            // round. Otherwise, this can race with the state sync process.
            if ledger_info.commit_info().round() > root.commit_info().round() {
                *root = ledger_info;
            }
        })
    }

    /// Creates a new observer subscription by sending subscription requests to
    /// appropriate peers and waiting for a successful response. If `previous_subscription_peer`
    /// is provided, it will be excluded from the selection process.
    async fn create_new_observer_subscription(
        &mut self,
        previous_subscription_peer: Option<PeerNetworkId>,
    ) {
        // Get a set of sorted peers to service our subscription request
        let sorted_peers = match self.sort_peers_for_subscription(previous_subscription_peer) {
            Some(sorted_peers) => sorted_peers,
            None => {
                error!(LogSchema::new(LogEntry::ConsensusObserver)
                    .message("Failed to sort peers for subscription requests!"));
                return;
            },
        };

        // Verify that we have potential peers
        if sorted_peers.is_empty() {
            warn!(LogSchema::new(LogEntry::ConsensusObserver)
                .message("There are no peers to subscribe to!"));
            return;
        }

        // Go through the sorted peers and attempt to subscribe to a single peer.
        // The first peer that responds successfully will be the selected peer.
        for selected_peer in &sorted_peers {
            info!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Attempting to subscribe to peer: {}!",
                    selected_peer
                ))
            );

            // Send a subscription request to the peer and wait for the response.
            // Note: it is fine to block here because we assume only a single active subscription.
            let subscription_request = ConsensusObserverRequest::Subscribe;
            let response = self
                .consensus_observer_client
                .send_rpc_request_to_peer(
                    selected_peer,
                    subscription_request,
                    self.consensus_observer_config.network_request_timeout_ms,
                )
                .await;

            // Process the response and update the active subscription
            match response {
                Ok(ConsensusObserverResponse::SubscribeAck) => {
                    info!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Successfully subscribed to peer: {}!",
                            selected_peer
                        ))
                    );

                    // Update the active subscription
                    let subscription = ConsensusObserverSubscription::new(
                        self.consensus_observer_config,
                        self.db_reader.clone(),
                        *selected_peer,
                        self.time_service.clone(),
                    );
                    self.active_observer_subscription = Some(subscription);

                    return; // Return after successfully subscribing
                },
                Ok(response) => {
                    // We received an invalid response
                    warn!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Got unexpected response type: {:?}",
                            response.get_label()
                        ))
                    );
                },
                Err(error) => {
                    // We encountered an error while sending the request
                    error!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Failed to send subscription request to peer: {}! Error: {:?}",
                            selected_peer, error
                        ))
                    );
                },
            }
        }

        // We failed to connect to any peers
        warn!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Failed to subscribe to any peers! Num peers attempted: {:?}",
                sorted_peers.len()
            ))
        );
    }

    /// Finalizes the ordered block by sending it to the execution pipeline
    async fn finalize_ordered_block(&mut self, ordered_block: OrderedBlock) {
        if let Err(error) = self
            .execution_client
            .finalize_order(
                ordered_block.blocks(),
                ordered_block.ordered_proof().clone(),
                self.create_commit_callback(),
            )
            .await
        {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to finalize ordered block! Error: {:?}",
                    error
                ))
            );
        }
    }

    /// Forwards the commit decision to the execution pipeline
    fn forward_commit_decision(&self, commit_decision: CommitDecision) {
        // Create a dummy RPC message
        let (response_sender, _response_receiver) = oneshot::channel();
        let commit_request = IncomingCommitRequest {
            req: CommitMessage::Decision(pipeline::commit_decision::CommitDecision::new(
                commit_decision.commit_proof().clone(),
            )),
            protocol: ProtocolId::ConsensusDirectSendCompressed,
            response_sender,
        };

        // Send the message to the execution client
        if let Err(error) = self
            .execution_client
            .send_commit_msg(AccountAddress::ONE, commit_request)
        {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to send commit decision to the execution pipeline! Error: {:?}",
                    error
                ))
            )
        };
    }

    /// Returns the current epoch state, and panics if it is not set
    fn get_epoch_state(&self) -> Arc<EpochState> {
        self.epoch_state
            .clone()
            .expect("The epoch state is not set! This should never happen!")
    }

    /// Returns the last known block
    fn get_last_block(&self) -> BlockInfo {
        if let Some(last_pending_block) = self.pending_ordered_blocks.get_last_pending_block() {
            last_pending_block
        } else {
            // Return the root ledger info
            self.root.lock().commit_info().clone()
        }
    }

    /// Gets the connected peers and metadata. If an error occurred,
    /// it is logged and None is returned.
    fn get_connected_peers_and_metadata(&self) -> Option<HashMap<PeerNetworkId, PeerMetadata>> {
        match self
            .consensus_observer_client
            .get_peers_and_metadata()
            .get_connected_peers_and_metadata()
        {
            Ok(connected_peers_and_metadata) => Some(connected_peers_and_metadata),
            Err(error) => {
                error!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to get connected peers and metadata! Error: {:?}",
                        error
                    ))
                );
                None
            },
        }
    }

    /// Processes the block payload
    fn process_block_payload(&mut self, block_payload: BlockPayload) {
        // Unpack the block payload
        let block = block_payload.block;
        let transactions = block_payload.transactions;
        let limit = block_payload.limit;

        // TODO: verify the block payload!

        // Update the payload store with the payload
        self.block_payload_store
            .insert_block_payload(block, transactions, limit);
    }

    /// Processes the commit decision
    fn process_commit_decision(&mut self, commit_decision: CommitDecision) {
        // If the commit decision is for the current epoch, verify it
        let epoch_state = self.get_epoch_state();
        let commit_decision_epoch = commit_decision.epoch();
        if commit_decision_epoch == epoch_state.epoch {
            // Verify the commit decision
            if let Err(error) = commit_decision.verify_commit_proof(&epoch_state) {
                error!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to verify commit decision! Ignoring: {:?}, Error: {:?}",
                        commit_decision.proof_block_info(),
                        error
                    ))
                );
                return;
            }

            // Update the pending blocks with the commit decision
            if self.process_commit_decision_for_pending_block(&commit_decision) {
                return; // The commit decision was successfully processed
            }
        }

        // TODO: identify the best way to handle an invalid commit decision
        // for a future epoch. In such cases, we currently rely on state sync.

        // Otherwise, we failed to process the commit decision. If the commit
        // is for a future epoch or round, we need to state sync.
        let commit_decision_round = commit_decision.round();
        let last_block = self.get_last_block();
        if commit_decision_epoch > last_block.epoch() || commit_decision_round > last_block.round()
        {
            info!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Started syncing to {}!",
                    commit_decision.proof_block_info()
                ))
            );

            // Update the root and clear the pending blocks (up to the commit)
            *self.root.lock() = commit_decision.commit_proof().clone();
            self.pending_ordered_blocks
                .remove_blocks_for_commit(commit_decision.commit_proof());

            // Start the state sync process
            let abort_handle = sync_to_commit_decision(
                commit_decision,
                commit_decision_epoch,
                commit_decision_round,
                self.execution_client.clone(),
                self.sync_notification_sender.clone(),
            );
            self.sync_handle = Some(DropGuard::new(abort_handle));
        }
    }

    /// Processes the commit decision for the pending block and returns true iff
    /// the commit decision was successfully processed. Note: this function
    /// assumes the commit decision has already been verified.
    fn process_commit_decision_for_pending_block(&self, commit_decision: &CommitDecision) -> bool {
        // Get the pending block for the commit decision
        let pending_block = self
            .pending_ordered_blocks
            .get_verified_pending_block(commit_decision.epoch(), commit_decision.round());

        // Process the pending block
        if let Some(pending_block) = pending_block {
            // If the payload exists, add the commit decision to the pending blocks
            if self
                .block_payload_store
                .all_payloads_exist(pending_block.blocks())
            {
                debug!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Adding decision to pending block: {}",
                        commit_decision.proof_block_info()
                    ))
                );
                self.pending_ordered_blocks
                    .update_commit_decision(commit_decision);

                // If we are not in sync mode, forward the commit decision to the execution pipeline
                if self.sync_handle.is_none() {
                    debug!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Forwarding commit decision to the execution pipeline: {}",
                            commit_decision.proof_block_info()
                        ))
                    );
                    self.forward_commit_decision(commit_decision.clone());
                }

                return true; // The commit decision was successfully processed
            }
        }

        false // The commit decision was not processed
    }

    /// Processes a direct send message
    async fn process_direct_send_message(
        &mut self,
        peer_network_id: PeerNetworkId,
        message: ConsensusObserverDirectSend,
    ) {
        // Verify the message is from the peer we've subscribed to
        if let Some(active_subscription) = &mut self.active_observer_subscription {
            if let Err(error) = active_subscription.verify_message_sender(&peer_network_id) {
                warn!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Message failed subscription sender verification! Error: {:?}",
                        error,
                    ))
                );

                // Send another unsubscription request to the peer
                self.unsubscribe_from_peer(peer_network_id);
                return;
            }
        } else {
            warn!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Received message from unexpected peer: {}! No active subscription found!",
                    peer_network_id
                ))
            );

            // Send an unsubscription request to the peer
            self.unsubscribe_from_peer(peer_network_id);
            return;
        };

        // Increment the received message counter
        metrics::increment_request_counter(
            &metrics::OBSERVER_RECEIVED_MESSAGES,
            message.get_label(),
            &peer_network_id,
        );

        // Process the message based on the type
        match message {
            ConsensusObserverDirectSend::OrderedBlock(ordered_block) => {
                debug!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Received ordered block: {}, from peer: {}!",
                        ordered_block.proof_block_info(),
                        peer_network_id
                    ))
                );
                self.process_ordered_block(ordered_block).await;
            },
            ConsensusObserverDirectSend::CommitDecision(commit_decision) => {
                debug!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Received commit decision: {}, from peer: {}!",
                        commit_decision.proof_block_info(),
                        peer_network_id
                    ))
                );
                self.process_commit_decision(commit_decision);
            },
            ConsensusObserverDirectSend::BlockPayload(block_payload) => {
                debug!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Received block payload: {}, from peer: {}!",
                        block_payload.block, peer_network_id
                    ))
                );
                self.process_block_payload(block_payload);
            },
        }
    }

    /// Processes the ordered block
    async fn process_ordered_block(&mut self, ordered_block: OrderedBlock) {
        // Verify the ordered blocks before processing
        if let Err(error) = ordered_block.verify_ordered_blocks() {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to verify ordered blocks! Ignoring: {:?}, Error: {:?}",
                    ordered_block.proof_block_info(),
                    error
                ))
            );
            return;
        };

        // If the ordered block is for the current epoch, verify the proof
        let epoch_state = self.get_epoch_state();
        let verified_ordered_proof =
            if ordered_block.proof_block_info().epoch() == epoch_state.epoch {
                // Verify the ordered proof
                if let Err(error) = ordered_block.verify_ordered_proof(&epoch_state) {
                    warn!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Failed to verify ordered proof! Ignoring: {:?}, Error: {:?}",
                            ordered_block.proof_block_info(),
                            error
                        ))
                    );
                    return;
                }

                true // We have successfully verified the proof
            } else {
                false // We can't verify the proof yet
            };

        // If the block is a child of our last block, we can insert it
        if self.get_last_block().id() == ordered_block.first_block().parent_id() {
            // Insert the ordered block into the pending blocks
            self.pending_ordered_blocks
                .insert_ordered_block(ordered_block.clone(), verified_ordered_proof);

            // If we verified the proof, and we're not in sync mode, finalize the ordered blocks
            if verified_ordered_proof && self.sync_handle.is_none() {
                debug!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Forwarding blocks to the execution pipeline: {}",
                        ordered_block.proof_block_info()
                    ))
                );

                // Finalize the ordered block
                self.finalize_ordered_block(ordered_block).await;
            }
        } else {
            warn!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Parent block is missing! Ignoring: {:?}",
                    ordered_block.proof_block_info()
                ))
            );
        }
    }

    /// Processes a request message
    fn process_request_message(
        &mut self,
        peer_network_id: PeerNetworkId,
        request: ConsensusObserverRequest,
        response_sender: Option<ResponseSender>,
    ) {
        // Ensure that the response sender is present
        let response_sender = match response_sender {
            Some(response_sender) => response_sender,
            None => {
                error!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Missing response sender for RCP request: {:?}",
                        request
                    ))
                );
                return; // Something has gone wrong!
            },
        };

        // Forward the request to the consensus publisher
        if let Some(consensus_publisher) = &self.consensus_publisher {
            consensus_publisher.handle_subscription_request(
                &peer_network_id,
                request,
                response_sender,
            );
        }
    }

    /// Processes the sync complete notification for the given epoch and round
    async fn process_sync_notification(&mut self, epoch: u64, round: Round) {
        // Log the sync notification
        info!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Received sync complete notification for epoch {}, round: {}",
                epoch, round
            ))
        );

        // Verify that the sync notification is for the current epoch and round
        if !check_root_epoch_and_round(self.root.clone(), epoch, round) {
            info!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Received invalid sync notification for epoch: {}, round: {}! Current root: {:?}",
                epoch, round, self.root
                ))
            );
            return;
        }

        // If the epoch has changed, end the current epoch and start the new one
        let current_epoch_state = self.get_epoch_state();
        if epoch > current_epoch_state.epoch {
            // Wait for the next epoch to start
            self.execution_client.end_epoch().await;
            self.wait_for_epoch_start().await;

            // Verify the pending blocks for the new epoch
            self.pending_ordered_blocks
                .verify_pending_blocks(&current_epoch_state);
        }

        // Reset and drop the sync handle
        self.sync_handle = None;

        // Process all the pending blocks. These were all buffered during the state sync process.
        for (_, (ordered_block, commit_decision)) in self
            .pending_ordered_blocks
            .get_all_verified_pending_blocks()
        {
            // Finalize the ordered block
            self.finalize_ordered_block(ordered_block).await;

            // If a commit decision is available, forward it to the execution pipeline
            if let Some(commit_decision) = commit_decision {
                self.forward_commit_decision(commit_decision.clone());
            }
        }
    }

    /// Produces a list of sorted peers to service our subscription request. Peers
    /// are prioritized by validator distance and latency.
    /// Note: if `previous_subscription_peer` is provided, it will be excluded
    /// from the selection process. Likewise, all peers currently subscribed to us
    /// will be excluded from the selection process.
    fn sort_peers_for_subscription(
        &mut self,
        previous_subscription_peer: Option<PeerNetworkId>,
    ) -> Option<Vec<PeerNetworkId>> {
        if let Some(mut peers_and_metadata) = self.get_connected_peers_and_metadata() {
            // Remove the previous subscription peer (if provided)
            if let Some(previous_subscription_peer) = previous_subscription_peer {
                let _ = peers_and_metadata.remove(&previous_subscription_peer);
            }

            // Remove any peers that are currently subscribed to us
            if let Some(consensus_publisher) = &self.consensus_publisher {
                for peer_network_id in consensus_publisher.get_active_subscribers() {
                    let _ = peers_and_metadata.remove(&peer_network_id);
                }
            }

            // Sort the peers by validator distance and latency
            let sorted_peers = subscription::sort_peers_by_distance_and_latency(peers_and_metadata);

            // Return the sorted peers
            Some(sorted_peers)
        } else {
            None // No connected peers were found
        }
    }

    /// Unsubscribes from the given peer by sending an unsubscribe request
    fn unsubscribe_from_peer(&self, peer_network_id: PeerNetworkId) {
        // Send an unsubscribe request to the peer and process the response.
        // Note: we execute this asynchronously, as we don't need to wait for the response.
        let consensus_observer_client = self.consensus_observer_client.clone();
        let consensus_observer_config = self.consensus_observer_config;
        tokio::spawn(async move {
            // Send the unsubscribe request to the peer
            let unsubscribe_request = ConsensusObserverRequest::Unsubscribe;
            let response = consensus_observer_client
                .send_rpc_request_to_peer(
                    &peer_network_id,
                    unsubscribe_request,
                    consensus_observer_config.network_request_timeout_ms,
                )
                .await;

            // Process the response
            match response {
                Ok(ConsensusObserverResponse::UnsubscribeAck) => {
                    info!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Successfully unsubscribed from peer: {}!",
                            peer_network_id
                        ))
                    );
                },
                Ok(response) => {
                    // We received an invalid response
                    warn!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Got unexpected response type: {:?}",
                            response.get_label()
                        ))
                    );
                },
                Err(error) => {
                    // We encountered an error while sending the request
                    error!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Failed to send unsubscribe request to peer: {}! Error: {:?}",
                            peer_network_id, error
                        ))
                    );
                },
            }
        });
    }

    /// Updates the subscription creation metrics for the given peer
    fn update_subscription_creation_metrics(&self, peer_network_id: PeerNetworkId) {
        // Set the number of active subscriptions
        metrics::set_gauge(
            &metrics::OBSERVER_NUM_ACTIVE_SUBSCRIPTIONS,
            &peer_network_id.network_id(),
            1,
        );

        // Update the number of created subscriptions
        metrics::increment_request_counter(
            &metrics::OBSERVER_CREATED_SUBSCRIPTIONS,
            metrics::CREATED_SUBSCRIPTION_LABEL,
            &peer_network_id,
        );
    }

    /// Updates the subscription termination metrics for the given peer
    fn update_subscription_termination_metrics(
        &self,
        peer_network_id: PeerNetworkId,
        error: Error,
    ) {
        // Reset the number of active subscriptions
        metrics::set_gauge(
            &metrics::OBSERVER_NUM_ACTIVE_SUBSCRIPTIONS,
            &peer_network_id.network_id(),
            0,
        );

        // Update the number of terminated subscriptions
        metrics::increment_request_counter(
            &metrics::OBSERVER_TERMINATED_SUBSCRIPTIONS,
            error.get_label(),
            &peer_network_id,
        );
    }

    /// Waits for a new epoch to start
    async fn wait_for_epoch_start(&mut self) {
        // Extract the epoch state and on-chain configs
        let (epoch_state, consensus_config, execution_config, randomness_config) = if let Some(
            reconfig_events,
        ) =
            &mut self.reconfig_events
        {
            extract_on_chain_configs(reconfig_events).await
        } else {
            panic!("Reconfig events are required to wait for a new epoch to start! Something has gone wrong!")
        };

        // Update the local epoch state
        self.epoch_state = Some(epoch_state.clone());
        info!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "New epoch started: {}. Updated the epoch state!",
                epoch_state.epoch
            ))
        );

        // Create the payload manager
        let payload_manager = if consensus_config.quorum_store_enabled() {
            PayloadManager::ConsensusObserver(
                self.block_payload_store.get_block_payloads(),
                self.consensus_publisher.clone(),
            )
        } else {
            PayloadManager::DirectMempool
        };

        // Start the new epoch
        let signer = Arc::new(ValidatorSigner::new(
            AccountAddress::ZERO,
            bls12381::PrivateKey::genesis(),
        ));
        let dummy_signer = Arc::new(DagCommitSigner::new(signer.clone()));
        let (_, rand_msg_rx) =
            aptos_channel::new::<AccountAddress, IncomingRandGenRequest>(QueueStyle::FIFO, 1, None);
        self.execution_client
            .start_epoch(
                epoch_state.clone(),
                dummy_signer,
                Arc::new(payload_manager),
                &consensus_config,
                &execution_config,
                &randomness_config,
                None,
                None,
                rand_msg_rx,
                0,
            )
            .await;
    }

    /// Starts the consensus observer loop that processes incoming
    /// network messages and ensures the observer is making progress.
    pub async fn start(
        mut self,
        mut network_service_events: ConsensusObserverNetworkEvents,
        mut sync_notification_listener: tokio::sync::mpsc::UnboundedReceiver<(u64, Round)>,
    ) {
        // If the consensus publisher is enabled but the observer is disabled,
        // we should only forward incoming requests to the consensus publisher.
        if self.consensus_observer_config.publisher_enabled
            && !self.consensus_observer_config.observer_enabled
        {
            self.start_publisher_forwarding(&mut network_service_events)
                .await;
            return; // We should never return from this function
        }

        // Create a progress check ticker
        let mut progress_check_interval = IntervalStream::new(interval(Duration::from_millis(
            self.consensus_observer_config.progress_check_interval_ms,
        )))
        .fuse();

        // Wait for the epoch to start
        self.wait_for_epoch_start().await;

        // Start the consensus observer loop
        info!(LogSchema::new(LogEntry::ConsensusObserver)
            .message("Starting the consensus observer loop!"));
        loop {
            tokio::select! {
                Some(network_message) = network_service_events.next() => {
                    // Unpack the network message
                    let NetworkMessage {
                        peer_network_id,
                        protocol_id: _,
                        consensus_observer_message,
                        response_sender,
                    } = network_message;

                    // Process the consensus observer message
                    match consensus_observer_message {
                        ConsensusObserverMessage::DirectSend(message) => {
                            self.process_direct_send_message(peer_network_id, message).await;
                        },
                        ConsensusObserverMessage::Request(request) => {
                            self.process_request_message(peer_network_id, request, response_sender);
                        },
                        _ => {
                            error!(LogSchema::new(LogEntry::ConsensusObserver)
                                .message(&format!("Received unexpected message from peer: {}", peer_network_id)));
                        },
                    }
                }
                Some((epoch, round)) = sync_notification_listener.recv() => {
                    self.process_sync_notification(epoch, round).await;
                },
                _ = progress_check_interval.select_next_some() => {
                    self.check_progress().await;
                }
            else => break,
            }
        }

        // Log the exit of the consensus observer loop
        error!(LogSchema::new(LogEntry::ConsensusObserver)
            .message("The consensus observer loop exited unexpectedly!"));
    }

    /// Starts the publisher forwarding loop that forwards incoming
    /// requests to the consensus publisher. The rest of the consensus
    /// observer functionality is disabled.
    async fn start_publisher_forwarding(
        &mut self,
        network_service_events: &mut ConsensusObserverNetworkEvents,
    ) {
        // TODO: identify if there's a cleaner way to handle this!

        // Start the consensus publisher forwarding loop
        info!(LogSchema::new(LogEntry::ConsensusObserver)
            .message("Starting the consensus publisher forwarding loop!"));
        loop {
            tokio::select! {
                Some(network_message) = network_service_events.next() => {
                    // Unpack the network message
                    let NetworkMessage {
                        peer_network_id,
                        protocol_id: _,
                        consensus_observer_message,
                        response_sender,
                    } = network_message;

                    // Process the consensus observer message
                    match consensus_observer_message {
                        ConsensusObserverMessage::Request(request) => {
                            self.process_request_message(peer_network_id, request, response_sender);
                        },
                        _ => {
                            error!(LogSchema::new(LogEntry::ConsensusObserver)
                                .message(&format!("Received unexpected message from peer: {}", peer_network_id)));
                        },
                    }
                }
            }
        }
    }
}

/// Checks that the epoch and round match the current root
fn check_root_epoch_and_round(
    root: Arc<Mutex<LedgerInfoWithSignatures>>,
    epoch: u64,
    round: Round,
) -> bool {
    // Get the expected epoch and round
    let root = root.lock();
    let expected_epoch = root.commit_info().epoch();
    let expected_round = root.commit_info().round();

    // Check if the expected epoch and round match
    expected_epoch == epoch && expected_round == round
}

/// A simple helper function that extracts the on-chain configs from the reconfig events
async fn extract_on_chain_configs(
    reconfig_events: &mut ReconfigNotificationListener<DbBackedOnChainConfig>,
) -> (
    Arc<EpochState>,
    OnChainConsensusConfig,
    OnChainExecutionConfig,
    OnChainRandomnessConfig,
) {
    // Fetch the next reconfiguration notification
    let reconfig_notification = reconfig_events
        .next()
        .await
        .expect("Failed to get reconfig notification!");

    // Extract the epoch state from the reconfiguration notification
    let on_chain_configs = reconfig_notification.on_chain_configs;
    let validator_set: ValidatorSet = on_chain_configs
        .get()
        .expect("Failed to get the validator set from the on-chain configs!");
    let epoch_state = Arc::new(EpochState {
        epoch: on_chain_configs.epoch(),
        verifier: (&validator_set).into(),
    });

    // Extract the consensus config (or use the default if it's missing)
    let onchain_consensus_config: anyhow::Result<OnChainConsensusConfig> = on_chain_configs.get();
    if let Err(error) = &onchain_consensus_config {
        error!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Failed to read on-chain consensus config! Error: {:?}",
                error
            ))
        );
    }
    let consensus_config = onchain_consensus_config.unwrap_or_default();

    // Extract the execution config (or use the default if it's missing)
    let onchain_execution_config: anyhow::Result<OnChainExecutionConfig> = on_chain_configs.get();
    if let Err(error) = &onchain_execution_config {
        error!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Failed to read on-chain execution config! Error: {:?}",
                error
            ))
        );
    }
    let execution_config =
        onchain_execution_config.unwrap_or_else(|_| OnChainExecutionConfig::default_if_missing());

    // Extract the randomness config (or use the default if it's missing)
    let onchain_randomness_config: anyhow::Result<RandomnessConfigMoveStruct> =
        on_chain_configs.get();
    if let Err(error) = &onchain_randomness_config {
        error!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Failed to read on-chain randomness config! Error: {:?}",
                error
            ))
        );
    }
    let onchain_randomness_config = onchain_randomness_config
        .and_then(OnChainRandomnessConfig::try_from)
        .unwrap_or_else(|_| OnChainRandomnessConfig::default_if_missing());

    // Return the extracted epoch state and on-chain configs
    (
        epoch_state,
        consensus_config,
        execution_config,
        onchain_randomness_config,
    )
}

/// Spawns a task to sync to the given commit decision and notifies
/// the consensus observer. Also, returns an abort handle to cancel the task.
fn sync_to_commit_decision(
    commit_decision: CommitDecision,
    decision_epoch: u64,
    decision_round: Round,
    execution_client: Arc<dyn TExecutionClient>,
    sync_notification_sender: UnboundedSender<(u64, Round)>,
) -> AbortHandle {
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    tokio::spawn(Abortable::new(
        async move {
            // Sync to the commit decision
            if let Err(error) = execution_client
                .clone()
                .sync_to(commit_decision.commit_proof().clone())
                .await
            {
                warn!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to sync to commit decision: {:?}! Error: {:?}",
                        commit_decision, error
                    ))
                );
            }

            // Notify the consensus observer that the sync is complete
            if let Err(error) = sync_notification_sender.send((decision_epoch, decision_round)) {
                error!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to send sync notification for decision epoch: {:?}, round: {:?}! Error: {:?}",
                        decision_epoch, decision_round, error
                    ))
                );
            }
        },
        abort_registration,
    ));
    abort_handle
}
