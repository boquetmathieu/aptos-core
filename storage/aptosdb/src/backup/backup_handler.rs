// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event_store::EventStore,
    ledger_store::LedgerStore,
    metrics::{
        BACKUP_EPOCH_ENDING_EPOCH, BACKUP_STATE_SNAPSHOT_LEAF_IDX, BACKUP_STATE_SNAPSHOT_VERSION,
        BACKUP_TXN_VERSION,
    },
    state_store::StateStore,
    transaction_store::TransactionStore,
};
use anyhow::{ensure, Result};
use aptos_crypto::hash::HashValue;
use aptos_jellyfish_merkle::iterator::JellyfishMerkleIterator;
use aptos_types::{
    contract_event::ContractEvent,
    ledger_info::LedgerInfoWithSignatures,
    proof::{SparseMerkleRangeProof, TransactionAccumulatorRangeProof, TransactionInfoWithProof},
    state_store::{state_key::StateKey, state_value::StateValue},
    transaction::{Transaction, TransactionInfo, Version},
};
use itertools::zip_eq;
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

/// `BackupHandler` provides functionalities for AptosDB data backup.
#[derive(Clone)]
pub struct BackupHandler {
    ledger_store: Arc<LedgerStore>,
    transaction_store: Arc<TransactionStore>,
    state_store: Arc<StateStore>,
    event_store: Arc<EventStore>,
}

impl BackupHandler {
    pub(crate) fn new(
        ledger_store: Arc<LedgerStore>,
        transaction_store: Arc<TransactionStore>,
        state_store: Arc<StateStore>,
        event_store: Arc<EventStore>,
    ) -> Self {
        Self {
            ledger_store,
            transaction_store,
            state_store,
            event_store,
        }
    }

    /// Gets an iterator that yields a range of transactions.
    pub fn get_transaction_iter(
        &self,
        start_version: Version,
        num_transactions: usize,
    ) -> Result<impl Iterator<Item = Result<(Transaction, TransactionInfo, Vec<ContractEvent>)>> + '_>
    {
        let txn_iter = self
            .transaction_store
            .get_transaction_iter(start_version, num_transactions)?;
        let txn_info_iter = self
            .ledger_store
            .get_transaction_info_iter(start_version, num_transactions)?;
        let events_iter = self
            .event_store
            .get_events_by_version_iter(start_version, num_transactions)?;

        let zipped = zip_eq(zip_eq(txn_iter, txn_info_iter), events_iter)
            .enumerate()
            .map(move |(idx, ((txn_res, txn_info_res), events_res))| {
                BACKUP_TXN_VERSION.set((start_version.wrapping_add(idx as u64)) as i64);
                Ok((txn_res?, txn_info_res?, events_res?))
            });
        Ok(zipped)
    }

    /// Gets the proof for a transaction chunk.
    /// N.B. the `LedgerInfo` returned will always be in the same epoch of the `last_version`.
    pub fn get_transaction_range_proof(
        &self,
        first_version: Version,
        last_version: Version,
    ) -> Result<(TransactionAccumulatorRangeProof, LedgerInfoWithSignatures)> {
        ensure!(
            last_version >= first_version,
            "Bad transaction range: [{}, {}]",
            first_version,
            last_version
        );
        let num_transactions = last_version - first_version + 1;
        let epoch = self.ledger_store.get_epoch(last_version)?;
        let ledger_info = self.ledger_store.get_latest_ledger_info_in_epoch(epoch)?;
        let accumulator_proof = self.ledger_store.get_transaction_range_proof(
            Some(first_version),
            num_transactions,
            ledger_info.ledger_info().version(),
        )?;
        Ok((accumulator_proof, ledger_info))
    }

    /// Gets an iterator which can yield all accounts in the state tree.
    pub fn get_account_iter(
        &self,
        version: Version,
    ) -> Result<Box<dyn Iterator<Item = Result<(StateKey, StateValue)>> + Send + Sync>> {
        let store = Arc::clone(&self.state_store);
        let iterator =
            JellyfishMerkleIterator::new(Arc::clone(&store), version, HashValue::zero())?
                .enumerate()
                .map(move |(idx, res)| {
                    BACKUP_STATE_SNAPSHOT_VERSION.set(version as i64);
                    BACKUP_STATE_SNAPSHOT_LEAF_IDX.set(idx as i64);
                    match res {
                        Ok((_hashed_key, key_and_version)) => Ok((
                            key_and_version.0.clone(),
                            store.get_value_at_version(&key_and_version)?,
                        )),
                        Err(err) => Err(err),
                    }
                });
        Ok(Box::new(iterator))
    }

    /// Gets the proof that proves a range of accounts.
    pub fn get_account_state_range_proof(
        &self,
        rightmost_key: HashValue,
        version: Version,
    ) -> Result<SparseMerkleRangeProof> {
        self.state_store
            .get_value_range_proof(rightmost_key, version)
    }

    /// Gets the epoch, committed version, and synced version of the DB.
    pub fn get_db_state(&self) -> Result<Option<DbState>> {
        self.ledger_store
            .get_startup_info()?
            .map(
                |(latest_li, epoch_state_if_not_in_li, synced_version_opt)| {
                    Ok(DbState {
                        epoch: latest_li
                            .ledger_info()
                            .next_epoch_state()
                            .unwrap_or_else(|| {
                                epoch_state_if_not_in_li
                                    .as_ref()
                                    .expect("EpochState must exist")
                            })
                            .epoch,
                        committed_version: latest_li.ledger_info().version(),
                        synced_version: synced_version_opt
                            .unwrap_or_else(|| latest_li.ledger_info().version()),
                    })
                },
            )
            .transpose()
    }

    /// Gets the proof of the state root at specified version.
    /// N.B. the `LedgerInfo` returned will always be in the same epoch of the version.
    pub fn get_state_root_proof(
        &self,
        version: Version,
    ) -> Result<(TransactionInfoWithProof, LedgerInfoWithSignatures)> {
        let epoch = self.ledger_store.get_epoch(version)?;
        let ledger_info = self.ledger_store.get_latest_ledger_info_in_epoch(epoch)?;
        let txn_info = self
            .ledger_store
            .get_transaction_info_with_proof(version, ledger_info.ledger_info().version())?;

        Ok((txn_info, ledger_info))
    }

    pub fn get_epoch_ending_ledger_info_iter(
        &self,
        start_epoch: u64,
        end_epoch: u64,
    ) -> Result<impl Iterator<Item = Result<LedgerInfoWithSignatures>> + '_> {
        Ok(self
            .ledger_store
            .get_epoch_ending_ledger_info_iter(start_epoch, end_epoch)?
            .enumerate()
            .map(move |(idx, li)| {
                BACKUP_EPOCH_ENDING_EPOCH.set((start_epoch + idx as u64) as i64);
                li
            }))
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DbState {
    pub epoch: u64,
    pub committed_version: Version,
    pub synced_version: Version,
}

impl fmt::Display for DbState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "epoch: {}, committed_version: {}, synced_version: {}",
            self.epoch, self.committed_version, self.synced_version,
        )
    }
}
