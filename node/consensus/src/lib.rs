// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![forbid(unsafe_code)]

#[macro_use]
extern crate tracing;

mod memory_pool;
pub use memory_pool::*;

#[cfg(test)]
mod tests;

use snarkos_account::Account;
use snarkos_node_narwhal::{
    helpers::{init_consensus_channels, ConsensusReceiver, PrimaryReceiver, PrimarySender, Storage as NarwhalStorage},
    BFT,
    MAX_GC_ROUNDS,
};
use snarkos_node_narwhal_committee::{Committee, MIN_STAKE};
use snarkos_node_narwhal_ledger_service::CoreLedgerService;
use snarkvm::{
    ledger::narwhal::{Data, Transmission, TransmissionID},
    prelude::{
        block::{Block, Transaction},
        coinbase::ProverSolution,
        store::ConsensusStorage,
        *,
    },
};

use ::rand::thread_rng;
use anyhow::Result;
use indexmap::IndexMap;
use parking_lot::Mutex;
use std::{future::Future, sync::Arc};
use tokio::{
    sync::{oneshot, OnceCell},
    task::JoinHandle,
};

#[derive(Clone)]
pub struct Consensus<N: Network, C: ConsensusStorage<N>> {
    /// The ledger.
    ledger: Ledger<N, C>,
    /// The BFT.
    bft: BFT<N>,
    /// The primary sender.
    primary_sender: Arc<OnceCell<PrimarySender<N>>>,
    /// The memory pool.
    memory_pool: MemoryPool<N>,
    /// The spawned handles.
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl<N: Network, C: ConsensusStorage<N>> Consensus<N, C> {
    /// Initializes a new instance of consensus.
    pub fn new(account: Account<N>, ledger: Ledger<N, C>, dev: Option<u16>) -> Result<Self> {
        // Initialize the committee.
        let committee = {
            // TODO (howardwu): Refactor committee out for narwhal.
            // TODO (howardwu): Fix the ledger round number.
            // TODO (howardwu): Retrieve the real committee members.
            // Sample the members.
            let mut members = IndexMap::new();
            for _ in 0..4 {
                members.insert(Address::<N>::new(thread_rng().gen()), MIN_STAKE);
            }
            Committee::new(ledger.latest_round() + 1, members)?
        };
        // Initialize the Narwhal storage.
        let storage = NarwhalStorage::new(committee, MAX_GC_ROUNDS);
        // Initialize the ledger service.
        let ledger_service = Box::new(CoreLedgerService::<N, C>::new(ledger.clone()));
        // Initialize the BFT.
        let bft = BFT::new(account, storage, ledger_service, None, dev)?;
        // Return the consensus.
        Ok(Self {
            ledger,
            bft,
            primary_sender: Default::default(),
            memory_pool: Default::default(),
            handles: Default::default(),
        })
    }

    /// Run the consensus instance.
    pub async fn run(&mut self, primary_sender: PrimarySender<N>, primary_receiver: PrimaryReceiver<N>) -> Result<()> {
        info!("Starting the consensus instance...");
        // Sets the primary sender.
        self.primary_sender.set(primary_sender.clone()).expect("Primary sender already set");
        // Initialize the consensus channels.
        let (consensus_sender, consensus_receiver) = init_consensus_channels();
        // Start the consensus.
        self.bft.run(primary_sender, primary_receiver, Some(consensus_sender)).await?;
        // Start the consensus handlers.
        self.start_handlers(consensus_receiver);
        Ok(())
    }

    /// Returns the ledger.
    pub const fn ledger(&self) -> &Ledger<N, C> {
        &self.ledger
    }

    /// Returns the BFT.
    pub const fn bft(&self) -> &BFT<N> {
        &self.bft
    }

    /// Returns the primary sender.
    pub fn primary_sender(&self) -> &PrimarySender<N> {
        self.primary_sender.get().expect("Primary sender not set")
    }

    /// Returns the number of unconfirmed transmissions.
    pub fn num_unconfirmed_transmissions(&self) -> usize {
        self.bft.num_unconfirmed_transmissions()
    }

    /// Returns the unconfirmed transmissions.
    pub fn unconfirmed_transmissions(&self) -> impl '_ + Iterator<Item = (TransmissionID<N>, Transmission<N>)> {
        self.bft.unconfirmed_transmissions()
    }

    /// Adds the given unconfirmed transaction to the memory pool.
    pub async fn add_unconfirmed_transaction(&self, transaction: Transaction<N>) -> Result<()> {
        // Initialize a callback sender and receiver.
        let (callback, callback_receiver) = oneshot::channel();
        // Send the transaction to the primary.
        self.primary_sender()
            .tx_unconfirmed_transaction
            .send((transaction.id(), Data::Object(transaction), callback))
            .await?;
        // Return the callback.
        callback_receiver.await?
    }

    /// Adds the given unconfirmed solution to the memory pool.
    pub async fn add_unconfirmed_solution(&self, solution: ProverSolution<N>) -> Result<()> {
        // Initialize a callback sender and receiver.
        let (callback, callback_receiver) = oneshot::channel();
        // Send the transaction to the primary.
        self.primary_sender()
            .tx_unconfirmed_solution
            .send((solution.commitment(), Data::Object(solution), callback))
            .await?;
        // Return the callback.
        callback_receiver.await?
    }

    /// Returns the memory pool.
    pub const fn memory_pool(&self) -> &MemoryPool<N> {
        &self.memory_pool
    }

    /// Returns `true` if the coinbase target is met.
    pub fn is_coinbase_target_met(&self) -> Result<bool> {
        // Retrieve the latest proof target.
        let latest_proof_target = self.ledger.latest_proof_target();
        // Compute the candidate coinbase target.
        let cumulative_proof_target = self.memory_pool.candidate_coinbase_target(latest_proof_target)?;
        // Retrieve the latest coinbase target.
        let latest_coinbase_target = self.ledger.latest_coinbase_target();
        // Check if the coinbase target is met.
        Ok(cumulative_proof_target >= latest_coinbase_target as u128)
    }

    /// Returns a candidate for the next block in the ledger.
    pub fn propose_next_block<R: Rng + CryptoRng>(&self, private_key: &PrivateKey<N>, rng: &mut R) -> Result<Block<N>> {
        // Retrieve the latest block.
        let latest_block = self.ledger.latest_block();
        // Retrieve the latest height.
        let latest_height = latest_block.height();
        // Retrieve the latest proof target.
        let latest_proof_target = latest_block.proof_target();
        // Retrieve the latest coinbase target.
        let latest_coinbase_target = latest_block.coinbase_target();

        // Select the transactions from the memory pool.
        let transactions = self.memory_pool.candidate_transactions(self);
        // Select the prover solutions from the memory pool.
        let prover_solutions =
            self.memory_pool.candidate_solutions(self, latest_height, latest_proof_target, latest_coinbase_target)?;

        // Prepare the next block.
        self.ledger.prepare_advance_to_next_block(private_key, transactions, prover_solutions, rng)
    }

    /// Advances the ledger to the next block.
    pub fn advance_to_next_block(&self, block: &Block<N>) -> Result<()> {
        // Adds the next block to the ledger.
        self.ledger.advance_to_next_block(block)?;

        // Clear the memory pool of unconfirmed transactions that are now invalid.
        self.memory_pool.clear_invalid_transactions(self);

        // If this starts a new epoch, clear all unconfirmed solutions from the memory pool.
        if block.epoch_number() > self.ledger.latest_epoch_number() {
            self.memory_pool.clear_all_unconfirmed_solutions();
        }
        // Otherwise, if a new coinbase was produced, clear the memory pool of unconfirmed solutions that are now invalid.
        else if block.coinbase().is_some() {
            self.memory_pool.clear_invalid_solutions(self);
        }

        info!("Advanced to block {}", block.height());
        Ok(())
    }
}

impl<N: Network, C: ConsensusStorage<N>> Consensus<N, C> {
    /// Starts the consensus handlers.
    fn start_handlers(&self, consensus_receiver: ConsensusReceiver<N>) {
        let ConsensusReceiver { mut rx_consensus_subdag } = consensus_receiver;

        // Process the committed subdag and transmissions from the BFT.
        let _self_ = self.clone();
        self.spawn(async move {
            while let Some((_committed_subdag, _transmissions)) = rx_consensus_subdag.recv().await {
                // TODO (howardwu): Prepare to create a new block.
            }
        });
    }

    /// Spawns a task with the given future; it should only be used for long-running tasks.
    fn spawn<T: Future<Output = ()> + Send + 'static>(&self, future: T) {
        self.handles.lock().push(tokio::spawn(future));
    }

    /// Shuts down the BFT.
    pub async fn shut_down(&self) {
        trace!("Shutting down consensus...");
        // Shut down the BFT.
        self.bft.shut_down().await;
        // Abort the tasks.
        self.handles.lock().iter().for_each(|handle| handle.abort());
    }
}
