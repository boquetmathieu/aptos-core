// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

//! This file defines state store APIs that are related account state Merkle tree.

#[cfg(test)]
mod state_store_test;

use crate::{
    change_set::ChangeSet,
    ledger_counters::LedgerCounter,
    schema::{
        jellyfish_merkle_node::JellyfishMerkleNodeSchema, stale_node_index::StaleNodeIndexSchema,
    },
    state_value::StateValueSchema,
    AptosDbError,
};
use anyhow::{anyhow, ensure, format_err, Result};
use aptos_crypto::{hash::CryptoHash, HashValue};
use aptos_infallible::Mutex;
use aptos_jellyfish_merkle::{
    iterator::JellyfishMerkleIterator, node_type::NodeKey, restore::StateSnapshotRestore,
    JellyfishMerkleTree, StateValueWriter, TreeReader, TreeWriter,
};
use aptos_types::{
    nibble::{nibble_path::NibblePath, ROOT_NIBBLE_HEIGHT},
    proof::{SparseMerkleProof, SparseMerkleRangeProof},
    state_store::{
        state_key::StateKey,
        state_key_prefix::StateKeyPrefix,
        state_value::{StateValue, StateValueChunkWithProof},
    },
    transaction::{Version, PRE_GENESIS_VERSION},
};
use schemadb::{SchemaBatch, DB};
use std::{cmp::Ordering, collections::HashMap, sync::Arc};
use storage_interface::StateSnapshotReceiver;

type LeafNode = aptos_jellyfish_merkle::node_type::LeafNode<StateKey>;
type Node = aptos_jellyfish_merkle::node_type::Node<StateKey>;
type NodeBatch = aptos_jellyfish_merkle::NodeBatch<StateKey>;
type StateValueBatch = aptos_jellyfish_merkle::StateValueBatch<StateKey, StateValue>;

pub const MAX_VALUES_TO_FETCH_FOR_KEY_PREFIX: usize = 10_000;

#[derive(Debug)]
pub(crate) struct StateStore {
    db: Arc<DB>,
    latest_version: Mutex<Option<Version>>,
}

impl StateStore {
    pub fn new(db: Arc<DB>) -> Self {
        let latest_version = Self::find_latest_persisted_version_from_db(&db, Version::MAX)
            .expect("Failed to query latest node on initialization.");

        Self {
            db,
            latest_version: Mutex::new(latest_version),
        }
    }

    pub fn latest_version(&self) -> Option<Version> {
        *self.latest_version.lock()
    }

    pub fn set_latest_state_checkpoint_version(&self, version: Version) {
        *self.latest_version.lock() = Some(version)
    }

    pub fn find_latest_persisted_version_less_than(
        &self,
        next_version: Version,
    ) -> Result<Option<Version>> {
        ensure!(
            next_version != PRE_GENESIS_VERSION,
            "Nothing before pre-genesis"
        );

        let latest_version_opt = self.latest_version();
        if let Some(latest_version) = &latest_version_opt {
            if *latest_version < next_version {
                return Ok(latest_version_opt);
            }
        }
        Self::find_latest_persisted_version_from_db(&self.db, next_version)
    }

    fn find_latest_persisted_version_from_db(
        db: &Arc<DB>,
        next_version: Version,
    ) -> Result<Option<Version>> {
        if next_version > 0 {
            let max_possible_version = next_version - 1;
            let mut iter = db.rev_iter::<JellyfishMerkleNodeSchema>(Default::default())?;
            iter.seek_for_prev(&NodeKey::new_empty_path(max_possible_version))?;
            if let Some((key, _node)) = iter.next().transpose()? {
                // TODO: If we break up a single update batch to multiple commits, we would need to
                // deal with a partial version, which hasn't got the root committed.
                return Ok(Some(key.version()));
            }
        }
        // try PRE_GENESIS
        Ok(db
            .get::<JellyfishMerkleNodeSchema>(&NodeKey::new_empty_path(PRE_GENESIS_VERSION))?
            .map(|_pre_genesis_root| PRE_GENESIS_VERSION))
    }

    /// Get the state value with proof given the state key and root hash of state Merkle tree
    pub fn get_value_with_proof_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<(Option<StateValue>, SparseMerkleProof)> {
        let (leaf_data, proof) =
            JellyfishMerkleTree::new(self).get_with_proof(state_key.hash(), version)?;
        Ok((
            match leaf_data {
                Some(x) => Some(self.get_value_at_version(&x.1)?),
                None => None,
            },
            proof,
        ))
    }

    /// Returns the key, value pairs for a particular state key prefix at at desired version. This
    /// API can be used to get all resources of an account by passing the account address as the
    /// key prefix.
    pub fn get_values_by_key_prefix(
        &self,
        key_prefix: &StateKeyPrefix,
        desired_version: Version,
    ) -> Result<HashMap<StateKey, StateValue>> {
        let mut iter = self.db.iter::<StateValueSchema>(Default::default())?;
        let mut result = HashMap::new();
        iter.seek(&(key_prefix))?;
        while let Some(((state_key, first_version), state_value)) = iter.next().transpose()? {
            // Cursor is currently at the first available version of the state key.
            // Check if the key_prefix is a valid prefix of the state_key we got from DB.

            if !key_prefix.is_prefix(&state_key)? {
                // No more keys matching the key_prefix, we can return the result.
                return Ok(result);
            }
            match first_version.cmp(&desired_version) {
                Ordering::Less => {
                    iter.seek_for_prev(&(state_key.clone(), desired_version))?;
                    let ((state_key, _), state_value) =
                        iter.next().transpose()?.ok_or_else(|| {
                            anyhow!(
                                "Failure seeking to desired version {:?} for state key {:?}",
                                desired_version,
                                state_key
                            )
                        })?;
                    result.insert(state_key.clone(), state_value);
                }

                Ordering::Equal => {
                    result.insert(state_key.clone(), state_value);
                }
                Ordering::Greater => {}
            }
            // We don't allow fetching arbitrarily large number of values to be fetched as this can
            // potentially slowdown the DB.
            if result.len() > MAX_VALUES_TO_FETCH_FOR_KEY_PREFIX {
                return Err(anyhow!(
                    "Too many values requested for key_prefix {:?} - maximum allowed {:?}",
                    key_prefix,
                    MAX_VALUES_TO_FETCH_FOR_KEY_PREFIX
                ));
            }
            // Seek to the next key - this can be done by seeking to the current key with max version
            iter.seek(&(state_key, u64::MAX))?;
        }
        Ok(result)
    }

    pub fn get_value_at_version(
        &self,
        key_and_version: &(StateKey, Version),
    ) -> Result<StateValue> {
        self.db
            .get::<StateValueSchema>(key_and_version)?
            .ok_or_else(|| {
                format_err!(
                    "Canot find value of key {:?} at version {}",
                    key_and_version.0,
                    key_and_version.1
                )
            })
    }

    /// Get the state value given the state key and root hash of state Merkle tree by using the
    /// state value index. Only used for testing for now but should replace the
    /// `get_value_with_proof_by_version` call for VM execution to fetch the value without proof.
    #[cfg(test)]
    pub fn get_value_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<Option<StateValue>> {
        let mut iter = self.db.iter::<StateValueSchema>(Default::default())?;
        iter.seek_for_prev(&(state_key.clone(), version))?;
        Ok(iter
            .next()
            .transpose()?
            .and_then(|((db_state_key, _), state_value)| {
                if *state_key == db_state_key {
                    Some(state_value)
                } else {
                    None
                }
            }))
    }

    /// Gets the proof that proves a range of accounts.
    pub fn get_value_range_proof(
        &self,
        rightmost_key: HashValue,
        version: Version,
    ) -> Result<SparseMerkleRangeProof> {
        JellyfishMerkleTree::new(self).get_range_proof(rightmost_key, version)
    }

    /// Put the `value_state_sets` into its own CF.
    pub fn put_value_sets(
        &self,
        value_state_sets: Vec<&HashMap<StateKey, StateValue>>,
        first_version: Version,
        cs: &mut ChangeSet,
    ) -> Result<()> {
        let kv_batch = value_state_sets
            .iter()
            .enumerate()
            .flat_map(|(i, kvs)| {
                kvs.iter()
                    .map(move |(k, v)| ((k.clone(), first_version + i as Version), v.clone()))
            })
            .collect::<HashMap<_, _>>();
        add_kv_batch(&mut cs.batch, &kv_batch)
    }

    /// Merklize the results generated by `value_state_sets` to `batch` and return the result root
    /// hashes for each write set.
    pub fn merklize_value_sets(
        &self,
        value_state_sets: Vec<&HashMap<StateKey, StateValue>>,
        node_hashes: Option<Vec<&HashMap<NibblePath, HashValue>>>,
        first_version: Version,
        cs: &mut ChangeSet,
    ) -> Result<Vec<HashValue>> {
        let value_sets: Vec<Vec<_>> = value_state_sets
            .iter()
            .map(|value_set| {
                value_set
                    .iter()
                    .map(|(key, value)| (key.hash(), (value.hash(), key.clone())))
                    .collect::<Vec<_>>()
            })
            .collect();

        let value_sets_ref = value_sets
            .iter()
            .map(|value_set| value_set.iter().map(|(x, y)| (*x, y)).collect::<Vec<_>>())
            .collect::<Vec<_>>();

        let (new_root_hash_vec, tree_update_batch) = JellyfishMerkleTree::new(self)
            .batch_put_value_sets(
                value_sets_ref,
                node_hashes,
                self.find_latest_persisted_version_less_than(first_version)?,
                first_version,
            )?;

        let num_versions = new_root_hash_vec.len();
        assert_eq!(num_versions, tree_update_batch.node_stats.len());

        tree_update_batch
            .node_stats
            .iter()
            .enumerate()
            .for_each(|(i, stats)| {
                let counter_bumps = cs.counter_bumps(first_version + i as u64);
                counter_bumps.bump(LedgerCounter::NewStateNodes, stats.new_nodes);
                counter_bumps.bump(LedgerCounter::NewStateLeaves, stats.new_leaves);
                counter_bumps.bump(LedgerCounter::StaleStateNodes, stats.stale_nodes);
                counter_bumps.bump(LedgerCounter::StaleStateLeaves, stats.stale_leaves);
            });

        add_node_batch(&mut cs.batch, &tree_update_batch.node_batch)?;

        tree_update_batch
            .stale_node_index_batch
            .iter()
            .map(|row| cs.batch.put::<StaleNodeIndexSchema>(row, &()))
            .collect::<Result<Vec<()>>>()?;

        Ok(new_root_hash_vec)
    }

    pub fn get_root_hash(&self, version: Version) -> Result<HashValue> {
        JellyfishMerkleTree::new(self).get_root_hash(version)
    }

    pub fn get_root_hash_option(&self, version: Version) -> Result<Option<HashValue>> {
        JellyfishMerkleTree::new(self).get_root_hash_option(version)
    }

    /// Finds the rightmost leaf by scanning the entire DB.
    #[cfg(test)]
    pub fn get_rightmost_leaf_naive(&self) -> Result<Option<(NodeKey, LeafNode)>> {
        let mut ret = None;

        let mut iter = self
            .db
            .iter::<JellyfishMerkleNodeSchema>(Default::default())?;
        iter.seek_to_first();

        while let Some((node_key, node)) = iter.next().transpose()? {
            if let Node::Leaf(leaf_node) = node {
                match ret {
                    None => ret = Some((node_key, leaf_node)),
                    Some(ref other) => {
                        if leaf_node.account_key() > other.1.account_key() {
                            ret = Some((node_key, leaf_node));
                        }
                    }
                }
            }
        }

        Ok(ret)
    }

    pub fn get_value_count(&self, version: Version) -> Result<usize> {
        JellyfishMerkleTree::new(self).get_leaf_count(version)
    }

    pub fn get_value_chunk_with_proof(
        self: &Arc<Self>,
        version: Version,
        first_index: usize,
        chunk_size: usize,
    ) -> Result<StateValueChunkWithProof> {
        let result_iter =
            JellyfishMerkleIterator::new_by_index(Arc::clone(self), version, first_index)?
                .take(chunk_size);
        let state_key_values: Vec<(StateKey, StateValue)> = result_iter
            .into_iter()
            .map(|res| res.and_then(|(_, k)| Ok((k.0.clone(), self.get_value_at_version(&k)?))))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !state_key_values.is_empty(),
            AptosDbError::NotFound(format!("State chunk starting at {}", first_index)),
        );
        let last_index = (state_key_values.len() - 1 + first_index) as u64;
        let first_key = state_key_values.first().expect("checked to exist").0.hash();
        let last_key = state_key_values.last().expect("checked to exist").0.hash();
        let proof = self.get_value_range_proof(last_key, version)?;
        let root_hash = self.get_root_hash(version)?;

        Ok(StateValueChunkWithProof {
            first_index: first_index as u64,
            last_index,
            first_key,
            last_key,
            raw_values: state_key_values,
            proof,
            root_hash,
        })
    }

    pub fn get_snapshot_receiver(
        self: &Arc<Self>,
        version: Version,
        expected_root_hash: HashValue,
    ) -> Result<Box<dyn StateSnapshotReceiver<StateKey, StateValue>>> {
        Ok(Box::new(StateSnapshotRestore::new_overwrite(
            Arc::clone(self),
            version,
            expected_root_hash,
        )?))
    }
}

impl TreeReader<StateKey> for StateStore {
    fn get_node_option(&self, node_key: &NodeKey) -> Result<Option<Node>> {
        self.db.get::<JellyfishMerkleNodeSchema>(node_key)
    }

    fn get_rightmost_leaf(&self) -> Result<Option<(NodeKey, LeafNode)>> {
        // Since everything has the same version during restore, we seek to the first node and get
        // its version.
        let mut iter = self
            .db
            .iter::<JellyfishMerkleNodeSchema>(Default::default())?;
        iter.seek_to_first();
        let version = match iter.next().transpose()? {
            Some((node_key, _node)) => node_key.version(),
            None => return Ok(None),
        };

        // The encoding of key and value in DB looks like:
        //
        // | <-------------- key --------------> | <- value -> |
        // | version | num_nibbles | nibble_path |    node     |
        //
        // Here version is fixed. For each num_nibbles, there could be a range of nibble paths
        // of the same length. If one of them is the rightmost leaf R, it must be at the end of this
        // range. Otherwise let's assume the R is in the middle of the range, so we
        // call the node at the end of this range X:
        //   1. If X is leaf, then X.account_key() > R.account_key(), because the nibble path is a
        //      prefix of the account key. So R is not the rightmost leaf.
        //   2. If X is internal node, then X must be on the right side of R, so all its children's
        //      account keys are larger than R.account_key(). So R is not the rightmost leaf.
        //
        // Given that num_nibbles ranges from 0 to ROOT_NIBBLE_HEIGHT, there are only
        // ROOT_NIBBLE_HEIGHT+1 ranges, so we can just find the node at the end of each range and
        // then pick the one with the largest account key.
        let mut ret = None;

        for num_nibbles in 1..=ROOT_NIBBLE_HEIGHT + 1 {
            let mut iter = self
                .db
                .iter::<JellyfishMerkleNodeSchema>(Default::default())?;
            // nibble_path is always non-empty except for the root, so if we use an empty nibble
            // path as the seek key, the iterator will end up pointing to the end of the previous
            // range.
            let seek_key = (version, num_nibbles as u8);
            iter.seek_for_prev(&seek_key)?;

            if let Some((node_key, node)) = iter.next().transpose()? {
                debug_assert_eq!(node_key.version(), version);
                debug_assert!(node_key.nibble_path().num_nibbles() < num_nibbles);

                if let Node::Leaf(leaf_node) = node {
                    match ret {
                        None => ret = Some((node_key, leaf_node)),
                        Some(ref other) => {
                            if leaf_node.account_key() > other.1.account_key() {
                                ret = Some((node_key, leaf_node));
                            }
                        }
                    }
                }
            }
        }

        Ok(ret)
    }
}

impl TreeWriter<StateKey> for StateStore {
    fn write_node_batch(&self, node_batch: &NodeBatch) -> Result<()> {
        let mut batch = SchemaBatch::new();
        add_node_batch(&mut batch, node_batch)?;
        self.db.write_schemas(batch)
    }

    fn finish_version(&self, version: Version) {
        self.set_latest_state_checkpoint_version(version)
    }
}

impl StateValueWriter<StateKey, StateValue> for StateStore {
    fn write_kv_batch(&self, node_batch: &StateValueBatch) -> Result<()> {
        let mut batch = SchemaBatch::new();
        add_kv_batch(&mut batch, node_batch)?;
        self.db.write_schemas(batch)
    }
}

fn add_node_batch(batch: &mut SchemaBatch, node_batch: &NodeBatch) -> Result<()> {
    node_batch
        .iter()
        .map(|(node_key, node)| batch.put::<JellyfishMerkleNodeSchema>(node_key, node))
        .collect::<Result<Vec<_>>>()?;

    // Add kv_batch
    Ok(())
}

fn add_kv_batch(batch: &mut SchemaBatch, kv_batch: &StateValueBatch) -> Result<()> {
    kv_batch
        .iter()
        .map(|(k, v)| batch.put::<StateValueSchema>(k, v))
        .collect::<Result<Vec<_>>>()?;

    // Add kv_batch
    Ok(())
}
