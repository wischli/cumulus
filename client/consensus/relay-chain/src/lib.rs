// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! The relay-chain provided consensus algorithm for parachains.
//!
//! This is the simplest consensus algorithm you can use when developing a parachain. It is a
//! permission-less consensus algorithm that doesn't require any staking or similar to join as a
//! collator. In this algorithm the consensus is provided by the relay-chain. This works in the
//! following way.
//!
//! 1. Each node that sees itself as a collator is free to build a parachain candidate.
//!
//! 2. This parachain candidate is send to the parachain validators that are part of the relay chain.
//!
//! 3. The parachain validators validate at most X different parachain candidates, where X is the
//! total number of parachain validators.
//!
//! 4. The parachain candidate that is backed by the most validators is chosen by the relay-chain
//! block producer to be added as backed candidate on chain.
//!
//! 5. After the parachain candidate got backed and included, all collators start at 1.

use cumulus_client_consensus_common::{
	ParachainBlockImport, ParachainCandidate, ParachainConsensus,
};
use cumulus_primitives_core::{relay_chain::v2::Hash as PHash, ParaId, PersistedValidationData};
use cumulus_relay_chain_interface::RelayChainInterface;

use sc_consensus::{BlockImport, BlockImportParams};
use sp_consensus::{
	BlockOrigin, EnableProofRecording, Environment, ProofRecording, Proposal, Proposer,
};
use sp_inherents::{CreateInherentDataProviders, InherentData, InherentDataProvider};
use sp_runtime::traits::{Block as BlockT, Header as HeaderT};

use parking_lot::Mutex;
use std::{sync::Arc, time::Duration};

mod import_queue;
pub use import_queue::{import_queue, Verifier};

const LOG_TARGET: &str = "cumulus-consensus-relay-chain";

/// The implementation of the relay-chain provided consensus for parachains.
pub struct RelayChainConsensus<B: BlockT, PF, BI, RCInterface, CIDP, BE> {
	para_id: ParaId,
	proposer_factory: Arc<Mutex<PF>>,
	create_inherent_data_providers: Arc<CIDP>,
	block_import: Arc<futures::lock::Mutex<ParachainBlockImport<B, BI, BE>>>,
	relay_chain_interface: RCInterface,
}

impl<B, PF, BI, RCInterface, CIDP, BE> Clone
	for RelayChainConsensus<B, PF, BI, RCInterface, CIDP, BE>
where
	B: BlockT,
	RCInterface: Clone,
{
	fn clone(&self) -> Self {
		Self {
			para_id: self.para_id,
			proposer_factory: self.proposer_factory.clone(),
			create_inherent_data_providers: self.create_inherent_data_providers.clone(),
			block_import: self.block_import.clone(),
			relay_chain_interface: self.relay_chain_interface.clone(),
		}
	}
}

impl<B, PF, BI, RCInterface, CIDP, BE> RelayChainConsensus<B, PF, BI, RCInterface, CIDP, BE>
where
	B: BlockT,
	RCInterface: RelayChainInterface,
	CIDP: CreateInherentDataProviders<B, (PHash, PersistedValidationData)>,
{
	/// Create a new instance of relay-chain provided consensus.
	pub fn new(
		para_id: ParaId,
		proposer_factory: PF,
		create_inherent_data_providers: CIDP,
		block_import: BI,
		relay_chain_interface: RCInterface,
		backend: Arc<BE>,
	) -> Self {
		Self {
			para_id,
			proposer_factory: Arc::new(Mutex::new(proposer_factory)),
			create_inherent_data_providers: Arc::new(create_inherent_data_providers),
			block_import: Arc::new(futures::lock::Mutex::new(ParachainBlockImport::new(
				block_import,
				backend,
			))),
			relay_chain_interface,
		}
	}

	/// Get the inherent data with validation function parameters injected
	async fn inherent_data(
		&self,
		parent: B::Hash,
		validation_data: &PersistedValidationData,
		relay_parent: PHash,
	) -> Option<InherentData> {
		let inherent_data_providers = self
			.create_inherent_data_providers
			.create_inherent_data_providers(parent, (relay_parent, validation_data.clone()))
			.await
			.map_err(|e| {
				tracing::error!(
					target: LOG_TARGET,
					error = ?e,
					"Failed to create inherent data providers.",
				)
			})
			.ok()?;

		inherent_data_providers
			.create_inherent_data()
			.map_err(|e| {
				tracing::error!(
					target: LOG_TARGET,
					error = ?e,
					"Failed to create inherent data.",
				)
			})
			.ok()
	}
}

#[async_trait::async_trait]
impl<B, PF, BI, RCInterface, CIDP, BE> ParachainConsensus<B>
	for RelayChainConsensus<B, PF, BI, RCInterface, CIDP, BE>
where
	B: BlockT,
	RCInterface: RelayChainInterface + Clone,
	BI: BlockImport<B> + Send + Sync,
	PF: Environment<B> + Send + Sync,
	PF::Proposer: Proposer<
		B,
		Transaction = BI::Transaction,
		ProofRecording = EnableProofRecording,
		Proof = <EnableProofRecording as ProofRecording>::Proof,
	>,
	CIDP: CreateInherentDataProviders<B, (PHash, PersistedValidationData)>,
	BE: sc_client_api::Backend<B> + Send + Sync,
{
	async fn produce_candidate(
		&mut self,
		parent: &B::Header,
		relay_parent: PHash,
		validation_data: &PersistedValidationData,
	) -> Option<ParachainCandidate<B>> {
		let proposer_future = self.proposer_factory.lock().init(&parent);

		let proposer = proposer_future
			.await
			.map_err(
				|e| tracing::error!(target: LOG_TARGET, error = ?e, "Could not create proposer."),
			)
			.ok()?;

		let inherent_data =
			self.inherent_data(parent.hash(), &validation_data, relay_parent).await?;

		let Proposal { block, storage_changes, proof } = proposer
			.propose(
				inherent_data,
				Default::default(),
				// TODO: Fix this.
				Duration::from_millis(500),
				// Set the block limit to 50% of the maximum PoV size.
				//
				// TODO: If we got benchmarking that includes that encapsulates the proof size,
				// we should be able to use the maximum pov size.
				Some((validation_data.max_pov_size / 2) as usize),
			)
			.await
			.map_err(|e| tracing::error!(target: LOG_TARGET, error = ?e, "Proposing failed."))
			.ok()?;

		let (header, extrinsics) = block.clone().deconstruct();

		let mut block_import_params = BlockImportParams::new(BlockOrigin::Own, header);
		block_import_params.body = Some(extrinsics);
		block_import_params.state_action = sc_consensus::StateAction::ApplyChanges(
			sc_consensus::StorageChanges::Changes(storage_changes),
		);

		if let Err(err) = self
			.block_import
			.lock()
			.await
			.import_block(block_import_params, Default::default())
			.await
		{
			tracing::error!(
				target: LOG_TARGET,
				at = ?parent.hash(),
				error = ?err,
				"Error importing build block.",
			);

			return None
		}

		Some(ParachainCandidate { block, proof })
	}
}

/// Parameters of [`build_relay_chain_consensus`].
pub struct BuildRelayChainConsensusParams<PF, BI, CIDP, RCInterface, BE> {
	pub para_id: ParaId,
	pub proposer_factory: PF,
	pub create_inherent_data_providers: CIDP,
	pub block_import: BI,
	pub relay_chain_interface: RCInterface,
	pub backend: Arc<BE>,
}

/// Build the [`RelayChainConsensus`].
///
/// Returns a boxed [`ParachainConsensus`].
pub fn build_relay_chain_consensus<Block, PF, BI, CIDP, RCInterface, BE>(
	BuildRelayChainConsensusParams {
		para_id,
		proposer_factory,
		create_inherent_data_providers,
		block_import,
		relay_chain_interface,
		backend,
	}: BuildRelayChainConsensusParams<PF, BI, CIDP, RCInterface, BE>,
) -> Box<dyn ParachainConsensus<Block>>
where
	Block: BlockT,
	PF: Environment<Block> + Send + Sync + 'static,
	PF::Proposer: Proposer<
		Block,
		Transaction = BI::Transaction,
		ProofRecording = EnableProofRecording,
		Proof = <EnableProofRecording as ProofRecording>::Proof,
	>,
	BI: BlockImport<Block> + Send + Sync + 'static,
	CIDP: CreateInherentDataProviders<Block, (PHash, PersistedValidationData)> + 'static,
	RCInterface: RelayChainInterface + Clone + 'static,
	BE: sc_client_api::Backend<Block> + Send + Sync + 'static,
{
	Box::new(RelayChainConsensus::new(
		para_id,
		proposer_factory,
		create_inherent_data_providers,
		block_import,
		relay_chain_interface,
		backend,
	))
}
