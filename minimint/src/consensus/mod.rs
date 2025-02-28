mod conflictfilter;

use crate::config::ServerConfig;
use crate::consensus::conflictfilter::ConflictFilterable;
use crate::db::{AcceptedTransactionKey, ProposedTransactionKey, ProposedTransactionKeyPrefix};
use crate::rng::RngGenerator;
use hbbft::honey_badger::Batch;
use minimint_api::db::batch::{BatchTx, DbBatch};
use minimint_api::db::{Database, RawDatabase};
use minimint_api::encoding::{Decodable, Encodable};
use minimint_api::outcome::OutputOutcome;
use minimint_api::transaction::{Input, OutPoint, Output, Transaction, TransactionError};
use minimint_api::{FederationModule, PeerId, TransactionId};
use minimint_derive::UnzipConsensus;
use minimint_mint::{Mint, MintError};
use minimint_wallet::{Wallet, WalletError};
use rand::{CryptoRng, RngCore};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, UnzipConsensus)]
pub enum ConsensusItem {
    Transaction(Transaction),
    Mint(<Mint as FederationModule>::ConsensusItem),
    Wallet(<Wallet as FederationModule>::ConsensusItem),
}

pub type HoneyBadgerMessage = hbbft::honey_badger::Message<PeerId>;
pub type ConsensusOutcome = Batch<Vec<ConsensusItem>, PeerId>;

pub struct FediMintConsensus<R>
where
    R: RngCore + CryptoRng,
{
    /// Cryptographic random number generator used for everything
    pub rng_gen: Box<dyn RngGenerator<Rng = R>>,
    /// Configuration describing the federation and containing our secrets
    pub cfg: ServerConfig, // TODO: make custom config

    /// Our local mint
    pub mint: Mint, // TODO: generate consensus code using Macro, making modules replaceable for testing and easy adaptability
    pub wallet: Wallet,

    /// KV Database into which all state is persisted to recover from in case of a crash
    pub db: Arc<dyn RawDatabase>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
struct AcceptedTransaction {
    epoch: u64,
    transaction: Transaction,
}

impl<R> FediMintConsensus<R>
where
    R: RngCore + CryptoRng,
{
    pub fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<(), TransactionSubmissionError> {
        let tx_hash = transaction.tx_hash();
        debug!("Received mint transaction {}", tx_hash);

        transaction.validate_funding(&self.cfg.fee_consensus)?;
        transaction.validate_signature()?;

        for input in &transaction.inputs {
            match input {
                Input::Coins(coins) => {
                    self.mint
                        .validate_input(coins)
                        .map_err(TransactionSubmissionError::InputCoinError)?;
                }
                Input::PegIn(peg_in) => {
                    self.wallet
                        .validate_input(peg_in)
                        .map_err(TransactionSubmissionError::InputPegIn)?;
                }
            }
        }

        for output in &transaction.outputs {
            match output {
                Output::Coins(coins) => {
                    self.mint
                        .validate_output(coins)
                        .map_err(TransactionSubmissionError::OutputCoinError)?;
                }
                Output::PegOut(peg_out) => {
                    self.wallet
                        .validate_output(peg_out)
                        .map_err(TransactionSubmissionError::OutputPegOut)?;
                }
            }
        }

        let new = self
            .db
            .insert_entry(&ProposedTransactionKey(tx_hash), &transaction)
            .expect("DB error");

        if new.is_some() {
            warn!("Added consensus item was already in consensus queue");
        }

        Ok(())
    }

    pub async fn process_consensus_outcome(&self, consensus_outcome: ConsensusOutcome) {
        let epoch = consensus_outcome.epoch;
        info!("Processing output of epoch {}", epoch);

        let UnzipConsensusItem {
            transaction: transaction_cis,
            wallet: wallet_cis,
            mint: mint_cis,
        } = consensus_outcome
            .contributions
            .into_iter()
            .flat_map(|(peer, cis)| cis.into_iter().map(move |ci| (peer, ci)))
            .unzip_consensus_item();

        let mut db_batch = DbBatch::new();
        self.wallet
            .begin_consensus_epoch(db_batch.transaction(), wallet_cis, self.rng_gen.get_rng())
            .await;
        self.mint
            .begin_consensus_epoch(db_batch.transaction(), mint_cis, self.rng_gen.get_rng())
            .await;
        self.db.apply_batch(db_batch).expect("DB error");

        // Since the changes to the database will happen all at once we won't be able to handle
        // conflicts between consensus items in one batch there. Thus we need to make sure that
        // all items in a batch are consistent/deterministically filter out inconsistent ones.
        // There are two item types that need checking:
        //  * peg-ins that each peg-in tx is only used to issue coins once
        //  * coin spends to avoid double spends in one batch
        let filtered_transactions = transaction_cis
            .into_iter()
            .filter_conflicts(|(_, tx)| tx)
            .collect::<Vec<_>>();

        // TODO: implement own parallel execution to avoid allocations and get rid of rayon
        let par_db_batches = filtered_transactions
            .into_par_iter()
            .map(|(peer, transaction)| {
                trace!(
                    "Processing transaction {:?} from peer {}",
                    transaction,
                    peer
                );
                let mut db_batch = DbBatch::new();
                db_batch.autocommit(|batch_tx| {
                    batch_tx.append_maybe_delete(ProposedTransactionKey(transaction.tx_hash()))
                });
                // TODO: use borrowed transaction
                match self.process_transaction(db_batch.transaction(), transaction.clone()) {
                    Ok(()) => {
                        db_batch.autocommit(|batch_tx| {
                            batch_tx.append_insert(
                                AcceptedTransactionKey(transaction.tx_hash()),
                                AcceptedTransaction { epoch, transaction },
                            );
                        });
                    }
                    Err(e) => {
                        // TODO: log error for user
                        warn!("Transaction proposed by peer {} failed: {}", peer, e);
                    }
                }

                db_batch
            })
            .collect::<Vec<_>>();
        let mut db_batch = DbBatch::new();
        db_batch.autocommit(|tx| tx.append_from_accumulators(par_db_batches.into_iter()));
        self.db.apply_batch(db_batch).expect("DB error");

        let mut db_batch = DbBatch::new();
        self.wallet
            .end_consensus_epoch(db_batch.transaction(), self.rng_gen.get_rng())
            .await;
        self.mint
            .end_consensus_epoch(db_batch.transaction(), self.rng_gen.get_rng())
            .await;
        self.db.apply_batch(db_batch).expect("DB error");
    }

    pub async fn get_consensus_proposal(&self) -> Vec<ConsensusItem> {
        self.db
            .find_by_prefix::<_, ProposedTransactionKey, _>(&ProposedTransactionKeyPrefix)
            .map(|res| {
                let (_key, value) = res.expect("DB error");
                ConsensusItem::Transaction(value)
            })
            .chain(
                self.wallet
                    .consensus_proposal(self.rng_gen.get_rng())
                    .await
                    .into_iter()
                    .map(|wci| ConsensusItem::Wallet(wci)),
            )
            .chain(
                self.mint
                    .consensus_proposal(self.rng_gen.get_rng())
                    .await
                    .into_iter()
                    .map(|mci| ConsensusItem::Mint(mci)),
            )
            .collect()
    }

    fn process_transaction(
        &self,
        mut batch: BatchTx,
        transaction: Transaction,
    ) -> Result<(), TransactionSubmissionError> {
        transaction.validate_funding(&self.cfg.fee_consensus)?;
        transaction.validate_signature()?;

        let tx_hash = transaction.tx_hash();

        for input in transaction.inputs {
            match input {
                Input::Coins(coins) => {
                    self.mint
                        .apply_input(batch.subtransaction(), &coins)
                        .map_err(TransactionSubmissionError::InputCoinError)?;
                }
                Input::PegIn(peg_in) => {
                    self.wallet
                        .apply_input(batch.subtransaction(), &peg_in)
                        .map_err(TransactionSubmissionError::InputPegIn)?;
                }
            }
        }

        for (idx, output) in transaction.outputs.into_iter().enumerate() {
            match output {
                Output::Coins(new_tokens) => {
                    self.mint
                        .apply_output(
                            batch.subtransaction(),
                            &new_tokens,
                            OutPoint {
                                txid: tx_hash,
                                out_idx: idx as u64,
                            },
                        )
                        .map_err(TransactionSubmissionError::OutputCoinError)?;
                }
                Output::PegOut(peg_out) => {
                    self.wallet
                        .apply_output(
                            batch.subtransaction(),
                            &peg_out,
                            OutPoint {
                                txid: tx_hash,
                                out_idx: idx as u64,
                            },
                        )
                        .map_err(TransactionSubmissionError::OutputPegOut)?;
                }
            }
        }

        batch.commit();
        Ok(())
    }

    pub fn transaction_status(
        &self,
        txid: TransactionId,
    ) -> Option<minimint_api::outcome::TransactionStatus> {
        let is_proposal = self
            .db
            .get_value::<_, Transaction>(&ProposedTransactionKey(txid))
            .expect("DB error")
            .is_some();

        let accepted: Option<AcceptedTransaction> = self
            .db
            .get_value::<_, AcceptedTransaction>(&AcceptedTransactionKey(txid))
            .expect("DB error");

        if let Some(accepted_tx) = accepted {
            let outputs = accepted_tx
                .transaction
                .outputs
                .iter()
                .enumerate()
                .map(|(out_idx, output)| {
                    let outpoint = OutPoint {
                        txid,
                        out_idx: out_idx as u64,
                    };
                    match output {
                        Output::Coins(_) => {
                            let outcome = self
                                .mint
                                .output_status(outpoint)
                                .expect("the transaction was processed, so should be known");
                            OutputOutcome::Mint(outcome)
                        }
                        Output::PegOut(_) => {
                            let outcome = self
                                .wallet
                                .output_status(outpoint)
                                .expect("the transaction was processed, so should be known");
                            OutputOutcome::Wallet(outcome)
                        }
                    }
                })
                .collect();

            Some(minimint_api::outcome::TransactionStatus::Accepted {
                epoch: accepted_tx.epoch,
                outputs,
            })
        } else if is_proposal {
            Some(minimint_api::outcome::TransactionStatus::AwaitingConsensus)
        } else {
            None
        }
    }
}

#[derive(Debug, Error)]
pub enum TransactionSubmissionError {
    #[error("High level transaction error: {0}")]
    TransactionError(TransactionError),
    #[error("Input coin error: {0}")]
    InputCoinError(MintError),
    #[error("Input peg-in error: {0}")]
    InputPegIn(WalletError),
    #[error("Output coin error: {0}")]
    OutputCoinError(MintError),
    #[error("Output coin error: {0}")]
    OutputPegOut(WalletError),
}

impl From<TransactionError> for TransactionSubmissionError {
    fn from(e: TransactionError) -> Self {
        TransactionSubmissionError::TransactionError(e)
    }
}
