// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! A [`SelectChain`] implementation designed for relay chains.
//!
//! This uses information about parachains to inform GRANDPA and BABE
//! about blocks which are safe to build on and blocks which are safe to
//! finalize.
//!
//! To learn more about chain-selection rules for Relay Chains, please see the
//! documentation on [chain-selection][chain-selection-guide]
//! in the implementers' guide.
//!
//! This is mostly a wrapper around a subsystem which implements the
//! chain-selection rule, which leaves the code to be very simple.
//!
//! However, this does apply the further finality constraints to the best
//! leaf returned from the chain selection subsystem by calling into other
//! subsystems which yield information about approvals and disputes.
//!
//! [chain-selection-guide]: https://w3f.github.io/parachain-implementers-guide/protocol-chain-selection.html

#![cfg(feature = "full-node")]

use polkadot_primitives::v1::{
	Hash, BlockNumber, Block as PolkadotBlock, Header as PolkadotHeader,
};
use polkadot_subsystem::messages::{ApprovalVotingMessage, HighestApprovedAncestorBlock, ChainSelectionMessage, DisputeCoordinatorMessage};
use polkadot_node_subsystem_util::metrics::{self, prometheus};
use futures::channel::oneshot;
use consensus_common::{Error as ConsensusError, SelectChain};
use std::sync::Arc;
use polkadot_overseer::{AllMessages, Handle, OverseerHandle};
use super::{HeaderProvider, HeaderProviderProvider};

/// The maximum amount of unfinalized blocks we are willing to allow due to approval checking
/// or disputes.
///
/// This is a safety net that should be removed at some point in the future.
const MAX_FINALITY_LAG: polkadot_primitives::v1::BlockNumber = 50;

const LOG_TARGET: &str = "parachain::chain-selection";

/// Prometheus metrics for chain-selection.
#[derive(Debug, Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

#[derive(Debug, Clone)]
struct MetricsInner {
	approval_checking_finality_lag: prometheus::Gauge<prometheus::U64>,
	disputes_finality_lag: prometheus::Gauge<prometheus::U64>,
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			approval_checking_finality_lag: prometheus::register(
				prometheus::Gauge::with_opts(
					prometheus::Opts::new(
						"parachain_approval_checking_finality_lag",
						"How far behind the head of the chain the Approval Checking protocol wants to vote",
					)
				)?,
				registry,
			)?,
			disputes_finality_lag: prometheus::register(
				prometheus::Gauge::with_opts(
					prometheus::Opts::new(
						"parachain_disputes_finality_lag",
						"How far behind the head of the chain the Disputes protocol wants to vote",
					)
				)?,
				registry,
			)?,
		};

		Ok(Metrics(Some(metrics)))
	}
}

impl Metrics {
	fn note_approval_checking_finality_lag(&self, lag: BlockNumber) {
		if let Some(ref metrics) = self.0 {
			metrics.approval_checking_finality_lag.set(lag as _);
		}
	}

	fn note_disputes_finality_lag(&self, lag: BlockNumber) {
		if let Some(ref metrics) = self.0 {
			metrics.disputes_finality_lag.set(lag as _);
		}
	}
}

/// A chain-selection implementation which provides safety for relay chains.
pub struct SelectRelayChainWithFallback<
	B: sc_client_api::Backend<PolkadotBlock>,
> {
	// A fallback to use in case the overseer is disconnected.
	//
	// This is used on relay chains which have not yet enabled
	// parachains as well as situations where the node is offline.
	fallback: sc_consensus::LongestChain<B, PolkadotBlock>,
	selection: SelectRelayChain<
		B,
		Handle,
	>,
}

impl<B> Clone for SelectRelayChainWithFallback<B>
where
	B: sc_client_api::Backend<PolkadotBlock>,
	SelectRelayChain<
		B,
		Handle,
	>: Clone,
{
	fn clone(&self) -> Self {
		Self {
			fallback: self.fallback.clone(),
			selection: self.selection.clone(),
		}
	}
}


impl<B> SelectRelayChainWithFallback<B>
where
	B: sc_client_api::Backend<PolkadotBlock> + 'static,
{
	/// Create a new [`SelectRelayChainWithFallback`] wrapping the given chain backend
	/// and a handle to the overseer.
	pub fn new(backend: Arc<B>, overseer: Handle, metrics: Metrics) -> Self {
		SelectRelayChainWithFallback {
			fallback: sc_consensus::LongestChain::new(backend.clone()),
			selection: SelectRelayChain::new(
				backend,
				overseer,
				metrics,
			),
		}
	}
}

impl<B> SelectRelayChainWithFallback<B>
where
	B: sc_client_api::Backend<PolkadotBlock> + 'static,
{
	/// Given an overseer handle, this connects the [`SelectRelayChainWithFallback`]'s
	/// internal handle and its clones to the same overseer.
	pub fn connect_to_overseer(
		&mut self,
		handle: OverseerHandle,
	) {
		self.selection.overseer.connect_to_overseer(handle);
	}
}


#[async_trait::async_trait]
impl<B> SelectChain<PolkadotBlock> for SelectRelayChainWithFallback<B>
where
	B: sc_client_api::Backend<PolkadotBlock> + 'static,
{
	async fn leaves(&self) -> Result<Vec<Hash>, ConsensusError> {
		if self.selection.overseer.is_disconnected() {
			return self.fallback.leaves().await
		}

		self.selection.leaves().await
	}

	async fn best_chain(&self) -> Result<PolkadotHeader, ConsensusError> {
		if self.selection.overseer.is_disconnected() {
			return self.fallback.best_chain().await
		}
		self.selection.best_chain().await
	}

	async fn finality_target(
		&self,
		target_hash: Hash,
		maybe_max_number: Option<BlockNumber>,
	) -> Result<Option<Hash>, ConsensusError> {
		if self.selection.overseer.is_disconnected() {
			return self.fallback.finality_target(target_hash, maybe_max_number).await
		}
		self.selection.finality_target(target_hash, maybe_max_number).await
	}
}


/// A chain-selection implementation which provides safety for relay chains
/// but does not handle situations where the overseer is not yet connected.
pub struct SelectRelayChain<B, OH> {
	backend: Arc<B>,
	overseer: OH,
	metrics: Metrics,
}

impl<B, OH> SelectRelayChain<B, OH>
where
	B: HeaderProviderProvider<PolkadotBlock>,
	OH: OverseerHandleT,
{
	/// Create a new [`SelectRelayChain`] wrapping the given chain backend
	/// and a handle to the overseer.
	pub fn new(backend: Arc<B>, overseer: OH, metrics: Metrics) -> Self {
		SelectRelayChain {
			backend,
			overseer,
			metrics,
		}
	}

	fn block_header(&self, hash: Hash) -> Result<PolkadotHeader, ConsensusError> {
		match HeaderProvider::header(self.backend.header_provider(), hash) {
			Ok(Some(header)) => Ok(header),
			Ok(None) => Err(ConsensusError::ChainLookup(format!(
				"Missing header with hash {:?}",
				hash,
			))),
			Err(e) => Err(ConsensusError::ChainLookup(format!(
				"Lookup failed for header with hash {:?}: {:?}",
				hash,
				e,
			))),
		}
	}

	fn block_number(&self, hash: Hash) -> Result<BlockNumber, ConsensusError> {
		match HeaderProvider::number(self.backend.header_provider(), hash) {
			Ok(Some(number)) => Ok(number),
			Ok(None) => Err(ConsensusError::ChainLookup(format!(
				"Missing number with hash {:?}",
				hash,
			))),
			Err(e) => Err(ConsensusError::ChainLookup(format!(
				"Lookup failed for number with hash {:?}: {:?}",
				hash,
				e,
			))),
		}
	}
}

impl<B, OH> Clone for SelectRelayChain<B, OH>
where
	B: HeaderProviderProvider<PolkadotBlock> + Send + Sync,
	OH: OverseerHandleT,
{
	fn clone(&self) -> Self {
		SelectRelayChain {
			backend: self.backend.clone(),
			overseer: self.overseer.clone(),
			metrics: self.metrics.clone(),
		}
	}
}

#[derive(thiserror::Error, Debug)]
enum Error {
	// A request to the subsystem was canceled.
	#[error("Overseer is disconnected from Chain Selection")]
	OverseerDisconnected(oneshot::Canceled),
	/// Chain selection returned empty leaves.
	#[error("ChainSelection returned no leaves")]
	EmptyLeaves,
}


/// Decoupling trait for the overseer handle.
///
/// Required for testing purposes.
#[async_trait::async_trait]
pub trait OverseerHandleT: Clone + Send + Sync {
	async fn send_msg<M: Send + Into<AllMessages>>(&mut self, msg: M, origin: &'static str);
}

#[async_trait::async_trait]
impl OverseerHandleT for Handle {
	async fn send_msg<M: Send + Into<AllMessages>>(&mut self, msg: M, origin: &'static str) {
		Handle::send_msg(self, msg, origin).await
	}
}


#[async_trait::async_trait]
impl<B, OH> SelectChain<PolkadotBlock> for SelectRelayChain<B, OH>
where
	B: HeaderProviderProvider<PolkadotBlock>,
	OH: OverseerHandleT,
{
	/// Get all leaves of the chain, i.e. block hashes that are suitable to
	/// build upon and have no suitable children.
	async fn leaves(&self) -> Result<Vec<Hash>, ConsensusError> {
		let (tx, rx) = oneshot::channel();

		self.overseer
			.clone()
			.send_msg(
				ChainSelectionMessage::Leaves(tx),
				std::any::type_name::<Self>(),
			).await;

		rx.await
			.map_err(Error::OverseerDisconnected)
			.map_err(|e| ConsensusError::Other(Box::new(e)))
	}

	/// Among all leaves, pick the one which is the best chain to build upon.
	async fn best_chain(&self) -> Result<PolkadotHeader, ConsensusError> {
		// The Chain Selection subsystem is supposed to treat the finalized
		// block as the best leaf in the case that there are no viable
		// leaves, so this should not happen in practice.
		let best_leaf = self.leaves()
			.await?
			.first()
			.ok_or_else(|| ConsensusError::Other(Box::new(Error::EmptyLeaves)))?
			.clone();


		self.block_header(best_leaf)
	}

	/// Get the best descendant of `target_hash` that we should attempt to
	/// finalize next, if any. It is valid to return the `target_hash` if
	/// no better block exists.
	///
	/// This will search all leaves to find the best one containing the
	/// given target hash, and then constrain to the given block number.
	///
	/// It will also constrain the chain to only chains which are fully
	/// approved, and chains which contain no disputes.
	async fn finality_target(
		&self,
		target_hash: Hash,
		maybe_max_number: Option<BlockNumber>,
	) -> Result<Option<Hash>, ConsensusError> {
		let mut overseer = self.overseer.clone();

		let subchain_head = {
			let (tx, rx) = oneshot::channel();
			overseer.send_msg(
				ChainSelectionMessage::BestLeafContaining(target_hash, tx),
				std::any::type_name::<Self>(),
			).await;

			let best = rx.await
				.map_err(Error::OverseerDisconnected)
				.map_err(|e| ConsensusError::Other(Box::new(e)))?;

			match best {
				// No viable leaves containing the block.
				None => return Ok(Some(target_hash)),
				Some(best) => best,
			}
		};

		let target_number = self.block_number(target_hash)?;

		// 1. Constrain the leaf according to `maybe_max_number`.
		let subchain_head = match maybe_max_number {
			None => subchain_head,
			Some(max) => {
				if max <= target_number {
					if max < target_number {
						tracing::warn!(
							LOG_TARGET,
							max_number = max,
							target_number,
							"`finality_target` max number is less than target number",
						);
					}
					return Ok(Some(target_hash));
				}
				// find the current number.
				let subchain_header = self.block_header(subchain_head)?;

				if subchain_header.number <= max {
					subchain_head
				} else {
					let (ancestor_hash, _) = crate::grandpa_support::walk_backwards_to_target_block(
						self.backend.header_provider(),
						max,
						&subchain_header,
					).map_err(|e| ConsensusError::ChainLookup(format!("{:?}", e)))?;

					ancestor_hash
				}
			}
		};

		let initial_leaf = subchain_head;
		let initial_leaf_number = self.block_number(initial_leaf)?;

		// 2. Constrain according to `ApprovedAncestor`.
		let (subchain_head, subchain_number, subchain_block_descriptions) = {

			let (tx, rx) = oneshot::channel();
			overseer.send_msg(
				ApprovalVotingMessage::ApprovedAncestor(
					subchain_head,
					target_number,
					tx,
				),
				std::any::type_name::<Self>(),
			).await;

			match rx.await
				.map_err(Error::OverseerDisconnected)
				.map_err(|e| ConsensusError::Other(Box::new(e)))?
			{
				// No approved ancestors means target hash is maximal vote.
				None => (target_hash, target_number, Vec::new()),
				Some(HighestApprovedAncestorBlock {
					number, hash, descriptions
				}) => (hash, number, descriptions),
			}
		};

		// Prevent sending flawed data to the dispute-coordinator.
		if Some(subchain_block_descriptions.len() as _) != subchain_number.checked_sub(target_number) {
			tracing::error!(
				LOG_TARGET,
				present_block_descriptions = subchain_block_descriptions.len(),
				target_number,
				subchain_number,
				"Mismatch of anticipated block descriptions and block number difference.",
			);
			return Ok(Some(target_hash));
		}

		let lag = initial_leaf_number.saturating_sub(subchain_number);
		self.metrics.note_approval_checking_finality_lag(lag);

		// 3. Constrain according to disputes:
		let (tx, rx) = oneshot::channel();
		overseer.send_msg(DisputeCoordinatorMessage::DetermineUndisputedChain{
				base_number: target_number,
				block_descriptions: subchain_block_descriptions,
				tx,
			},
			std::any::type_name::<Self>(),
		).await;
		let (subchain_number, subchain_head) = rx.await
			.map_err(Error::OverseerDisconnected)
			.map_err(|e| ConsensusError::Other(Box::new(e)))?
			.unwrap_or_else(|| (subchain_number, subchain_head));

		// The the total lag accounting for disputes.
		let lag_disputes = initial_leaf_number.saturating_sub(subchain_number);
		self.metrics.note_disputes_finality_lag(lag_disputes);

		// 4. Apply the maximum safeguard to the finality lag.
		if lag > MAX_FINALITY_LAG {
			// We need to constrain our vote as a safety net to
			// ensure the network continues to finalize.
			let safe_target = initial_leaf_number - MAX_FINALITY_LAG;

			if safe_target <= target_number {
				// Minimal vote needs to be on the target number.
				Ok(Some(target_hash))
			} else {
				// Otherwise we're looking for a descendant.
				let initial_leaf_header = self.block_header(initial_leaf)?;
				let (forced_target, _) = crate::grandpa_support::walk_backwards_to_target_block(
					self.backend.header_provider(),
					safe_target,
					&initial_leaf_header,
				).map_err(|e| ConsensusError::ChainLookup(format!("{:?}", e)))?;

				Ok(Some(forced_target))
			}
		} else {
			Ok(Some(subchain_head))
		}
	}
}
