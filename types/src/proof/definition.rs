// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

//! This module has definition of various proofs.

use super::{
    accumulator::InMemoryAccumulator, position::Position, verify_transaction_info,
    MerkleTreeInternalNode, SparseMerkleInternalNode, SparseMerkleLeafNode,
};
use crate::{
    ledger_info::LedgerInfo,
    state_store::state_value::StateValue,
    transaction::{TransactionInfo, Version},
};
use anyhow::{bail, ensure, format_err, Context, Result};
#[cfg(any(test, feature = "fuzzing"))]
use aptos_crypto::hash::TestOnlyHasher;
use aptos_crypto::{
    hash::{
        CryptoHash, CryptoHasher, EventAccumulatorHasher, TransactionAccumulatorHasher,
        SPARSE_MERKLE_PLACEHOLDER_HASH,
    },
    HashValue,
};
#[cfg(any(test, feature = "fuzzing"))]
use proptest_derive::Arbitrary;
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;

/// A proof that can be used authenticate an element in an accumulator given trusted root hash. For
/// example, both `LedgerInfoToTransactionInfoProof` and `TransactionInfoToEventProof` can be
/// constructed on top of this structure.
#[derive(Clone, Serialize, Deserialize)]
pub struct AccumulatorProof<H> {
    /// All siblings in this proof, including the default ones. Siblings are ordered from the bottom
    /// level to the root level.
    siblings: Vec<HashValue>,

    phantom: PhantomData<H>,
}

/// Because leaves can only take half the space in the tree, any numbering of the tree leaves must
/// not take the full width of the total space.  Thus, for a 64-bit ordering, our maximumm proof
/// depth is limited to 63.
pub type LeafCount = u64;
pub const MAX_ACCUMULATOR_PROOF_DEPTH: usize = 63;
pub const MAX_ACCUMULATOR_LEAVES: LeafCount = 1 << MAX_ACCUMULATOR_PROOF_DEPTH;

impl<H> AccumulatorProof<H>
where
    H: CryptoHasher,
{
    /// Constructs a new `AccumulatorProof` using a list of siblings.
    pub fn new(siblings: Vec<HashValue>) -> Self {
        AccumulatorProof {
            siblings,
            phantom: PhantomData,
        }
    }

    /// Returns the list of siblings in this proof.
    pub fn siblings(&self) -> &[HashValue] {
        &self.siblings
    }

    /// Verifies an element whose hash is `element_hash` and version is `element_version` exists in
    /// the accumulator whose root hash is `expected_root_hash` using the provided proof.
    pub fn verify(
        &self,
        expected_root_hash: HashValue,
        element_hash: HashValue,
        element_index: u64,
    ) -> Result<()> {
        ensure!(
            self.siblings.len() <= MAX_ACCUMULATOR_PROOF_DEPTH,
            "Accumulator proof has more than {} ({}) siblings.",
            MAX_ACCUMULATOR_PROOF_DEPTH,
            self.siblings.len()
        );

        let actual_root_hash = self
            .siblings
            .iter()
            .fold(
                (element_hash, element_index),
                // `index` denotes the index of the ancestor of the element at the current level.
                |(hash, index), sibling_hash| {
                    (
                        if index % 2 == 0 {
                            // the current node is a left child.
                            MerkleTreeInternalNode::<H>::new(hash, *sibling_hash).hash()
                        } else {
                            // the current node is a right child.
                            MerkleTreeInternalNode::<H>::new(*sibling_hash, hash).hash()
                        },
                        // The index of the parent at its level.
                        index / 2,
                    )
                },
            )
            .0;
        ensure!(
            actual_root_hash == expected_root_hash,
            "Root hashes do not match. Actual root hash: {:x}. Expected root hash: {:x}.",
            actual_root_hash,
            expected_root_hash
        );

        Ok(())
    }
}

impl<H> std::fmt::Debug for AccumulatorProof<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AccumulatorProof {{ siblings: {:?} }}", self.siblings)
    }
}

impl<H> PartialEq for AccumulatorProof<H> {
    fn eq(&self, other: &Self) -> bool {
        self.siblings == other.siblings
    }
}

impl<H> Eq for AccumulatorProof<H> {}

pub type TransactionAccumulatorProof = AccumulatorProof<TransactionAccumulatorHasher>;
pub type EventAccumulatorProof = AccumulatorProof<EventAccumulatorHasher>;
#[cfg(any(test, feature = "fuzzing"))]
pub type TestAccumulatorProof = AccumulatorProof<TestOnlyHasher>;

/// A proof that can be used to authenticate an element in a Sparse Merkle Tree given trusted root
/// hash. For example, `TransactionInfoToAccountProof` can be constructed on top of this structure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SparseMerkleProof {
    /// This proof can be used to authenticate whether a given leaf exists in the tree or not.
    ///     - If this is `Some(leaf_node)`
    ///         - If `leaf_node.key` equals requested key, this is an inclusion proof and
    ///           `leaf_node.value_hash` equals the hash of the corresponding account blob.
    ///         - Otherwise this is a non-inclusion proof. `leaf_node.key` is the only key
    ///           that exists in the subtree and `leaf_node.value_hash` equals the hash of the
    ///           corresponding account blob.
    ///     - If this is `None`, this is also a non-inclusion proof which indicates the subtree is
    ///       empty.
    leaf: Option<SparseMerkleLeafNode>,

    /// All siblings in this proof, including the default ones. Siblings are ordered from the bottom
    /// level to the root level.
    siblings: Vec<HashValue>,
}

impl SparseMerkleProof {
    /// Constructs a new `SparseMerkleProof` using leaf and a list of siblings.
    pub fn new(leaf: Option<SparseMerkleLeafNode>, siblings: Vec<HashValue>) -> Self {
        SparseMerkleProof { leaf, siblings }
    }

    /// Returns the leaf node in this proof.
    pub fn leaf(&self) -> Option<SparseMerkleLeafNode> {
        self.leaf
    }

    /// Returns the list of siblings in this proof.
    pub fn siblings(&self) -> &[HashValue] {
        &self.siblings
    }

    pub fn verify<V: CryptoHash>(
        &self,
        expected_root_hash: HashValue,
        element_key: HashValue,
        element_value: Option<&V>,
    ) -> Result<()> {
        self.verify_by_hash(
            expected_root_hash,
            element_key,
            element_value.map(|v| v.hash()),
        )
    }

    /// If `element_hash` is present, verifies an element whose key is `element_key` and value is
    /// authenticated by `element_hash` exists in the Sparse Merkle Tree using the provided proof.
    /// Otherwise verifies the proof is a valid non-inclusion proof that shows this key doesn't
    /// exist in the tree.
    pub fn verify_by_hash(
        &self,
        expected_root_hash: HashValue,
        element_key: HashValue,
        element_hash: Option<HashValue>,
    ) -> Result<()> {
        ensure!(
            self.siblings.len() <= HashValue::LENGTH_IN_BITS,
            "Sparse Merkle Tree proof has more than {} ({}) siblings.",
            HashValue::LENGTH_IN_BITS,
            self.siblings.len(),
        );

        match (element_hash, self.leaf) {
            (Some(hash), Some(leaf)) => {
                // This is an inclusion proof, so the key and value hash provided in the proof
                // should match element_key and element_value_hash. `siblings` should prove the
                // route from the leaf node to the root.
                ensure!(
                    element_key == leaf.key,
                    "Keys do not match. Key in proof: {:x}. Expected key: {:x}.",
                    leaf.key,
                    element_key
                );
                ensure!(
                    hash == leaf.value_hash,
                    "Value hashes do not match. Value hash in proof: {:x}. \
                     Expected value hash: {:x}",
                    leaf.value_hash,
                    hash,
                );
            }
            (Some(_hash), None) => bail!("Expected inclusion proof. Found non-inclusion proof."),
            (None, Some(leaf)) => {
                // This is a non-inclusion proof. The proof intends to show that if a leaf node
                // representing `element_key` is inserted, it will break a currently existing leaf
                // node represented by `proof_key` into a branch. `siblings` should prove the
                // route from that leaf node to the root.
                ensure!(
                    element_key != leaf.key,
                    "Expected non-inclusion proof, but key exists in proof.",
                );
                ensure!(
                    element_key.common_prefix_bits_len(leaf.key) >= self.siblings.len(),
                    "Key would not have ended up in the subtree where the provided key in proof \
                     is the only existing key, if it existed. So this is not a valid \
                     non-inclusion proof.",
                );
            }
            (None, None) => {
                // This is a non-inclusion proof. The proof intends to show that if a leaf node
                // representing `element_key` is inserted, it will show up at a currently empty
                // position. `sibling` should prove the route from this empty position to the root.
            }
        }

        let current_hash = self
            .leaf
            .map_or(*SPARSE_MERKLE_PLACEHOLDER_HASH, |leaf| leaf.hash());
        let actual_root_hash = self
            .siblings
            .iter()
            .zip(
                element_key
                    .iter_bits()
                    .rev()
                    .skip(HashValue::LENGTH_IN_BITS - self.siblings.len()),
            )
            .fold(current_hash, |hash, (sibling_hash, bit)| {
                if bit {
                    SparseMerkleInternalNode::new(*sibling_hash, hash).hash()
                } else {
                    SparseMerkleInternalNode::new(hash, *sibling_hash).hash()
                }
            });
        ensure!(
            actual_root_hash == expected_root_hash,
            "Root hashes do not match. Actual root hash: {:x}. Expected root hash: {:x}.",
            actual_root_hash,
            expected_root_hash,
        );

        Ok(())
    }
}

/// An in-memory accumulator for storing a summary of the core transaction info
/// accumulator. It is a summary in the sense that it only stores maximally
/// frozen subtree nodes rather than storing all leaves and internal nodes.
///
/// Light clients and light nodes use this type to store their currently verified
/// view of the transaction accumulator. When verifying state proofs, these clients
/// attempt to extend their accumulator summary with an [`AccumulatorConsistencyProof`]
/// to verifiably ratchet their trusted view of the accumulator to a newer state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TransactionAccumulatorSummary(InMemoryAccumulator<TransactionAccumulatorHasher>);

impl TransactionAccumulatorSummary {
    pub fn new(accumulator: InMemoryAccumulator<TransactionAccumulatorHasher>) -> Result<Self> {
        ensure!(
            !accumulator.is_empty(),
            "empty accumulator: we can't verify consistency proofs from an empty accumulator",
        );
        Ok(Self(accumulator))
    }

    pub fn version(&self) -> Version {
        self.0.version()
    }

    pub fn root_hash(&self) -> HashValue {
        self.0.root_hash()
    }

    /// Verify that this accumulator summary is "consistent" with the given
    /// [`LedgerInfo`], i.e., they both have the same version and accumulator
    /// root hash.
    pub fn verify_consistency(&self, ledger_info: &LedgerInfo) -> Result<()> {
        ensure!(
            ledger_info.version() == self.version(),
            "ledger info and accumulator must be at the same version: \
             ledger info version={}, accumulator version={}",
            ledger_info.version(),
            self.version(),
        );
        ensure!(
            ledger_info.transaction_accumulator_hash() == self.root_hash(),
            "ledger info root hash and accumulator root hash must match: \
             ledger info root hash={}, accumulator root hash={}",
            ledger_info.transaction_accumulator_hash(),
            self.root_hash(),
        );
        Ok(())
    }

    /// Try to build an accumulator summary using a consistency proof starting
    /// from pre-genesis.
    pub fn try_from_genesis_proof(
        genesis_proof: AccumulatorConsistencyProof,
        target_version: Version,
    ) -> Result<Self> {
        let num_txns = target_version.saturating_add(1);
        Ok(Self(InMemoryAccumulator::new(
            genesis_proof.into_subtrees(),
            num_txns,
        )?))
    }

    /// Try to extend an existing accumulator summary with a consistency proof
    /// starting from our current version. Then validate that the resulting
    /// accumulator summary is consistent with the given target [`LedgerInfo`].
    pub fn try_extend_with_proof(
        &self,
        consistency_proof: &AccumulatorConsistencyProof,
        target_li: &LedgerInfo,
    ) -> Result<Self> {
        ensure!(
            target_li.version() >= self.0.version(),
            "target ledger info version ({}) must be newer than our current accumulator \
             summary version ({})",
            target_li.version(),
            self.0.version(),
        );
        let num_new_txns = target_li.version() - self.0.version();
        let new_accumulator = Self(
            self.0
                .append_subtrees(consistency_proof.subtrees(), num_new_txns)?,
        );
        new_accumulator
            .verify_consistency(target_li)
            .context("accumulator is not consistent with the target ledger info after applying consistency proof")?;
        Ok(new_accumulator)
    }
}

/// A proof that can be used to show that two Merkle accumulators are consistent -- the big one can
/// be obtained by appending certain leaves to the small one. For example, at some point in time a
/// client knows that the root hash of the ledger at version 10 is `old_root` (it could be a
/// waypoint). If a server wants to prove that the new ledger at version `N` is derived from the
/// old ledger the client knows, it can show the subtrees that represent all the new leaves. If
/// the client can verify that it can indeed obtain the new root hash by appending these new
/// leaves, it can be convinced that the two accumulators are consistent.
///
/// See [`crate::proof::accumulator::InMemoryAccumulator::append_subtrees`] for more details.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccumulatorConsistencyProof {
    /// The subtrees representing the newly appended leaves.
    subtrees: Vec<HashValue>,
}

impl AccumulatorConsistencyProof {
    /// Constructs a new `AccumulatorConsistencyProof` using given `subtrees`.
    pub fn new(subtrees: Vec<HashValue>) -> Self {
        Self { subtrees }
    }

    pub fn is_empty(&self) -> bool {
        self.subtrees.is_empty()
    }

    /// Returns the subtrees.
    pub fn subtrees(&self) -> &[HashValue] {
        &self.subtrees
    }

    pub fn into_subtrees(self) -> Vec<HashValue> {
        self.subtrees
    }
}

/// A proof that is similar to `AccumulatorProof`, but can be used to authenticate a range of
/// leaves. For example, given the following accumulator:
///
/// ```text
///                 root
///                /     \
///              /         \
///            /             \
///           o               o
///         /   \           /   \
///        /     \         /     \
///       X       o       o       Y
///      / \     / \     / \     / \
///     o   o   a   b   c   Z   o   o
/// ```
///
/// if the proof wants to show that `[a, b, c]` exists in the accumulator, it would need `X` on the
/// left and `Y` and `Z` on the right.
#[derive(Clone, Deserialize, Serialize)]
pub struct AccumulatorRangeProof<H> {
    /// The siblings on the left of the path from the first leaf to the root. Siblings near the root
    /// are at the beginning of the vector.
    left_siblings: Vec<HashValue>,

    /// The sliblings on the right of the path from the last leaf to the root. Siblings near the root
    /// are at the beginning of the vector.
    right_siblings: Vec<HashValue>,

    phantom: PhantomData<H>,
}

impl<H> AccumulatorRangeProof<H>
where
    H: CryptoHasher,
{
    /// Constructs a new `AccumulatorRangeProof` using `left_siblings` and `right_siblings`.
    pub fn new(left_siblings: Vec<HashValue>, right_siblings: Vec<HashValue>) -> Self {
        Self {
            left_siblings,
            right_siblings,
            phantom: PhantomData,
        }
    }

    /// Constructs a new `AccumulatorRangeProof` for an empty list of leaves.
    pub fn new_empty() -> Self {
        Self::new(vec![], vec![])
    }

    /// Get all the left siblngs.
    pub fn left_siblings(&self) -> &Vec<HashValue> {
        &self.left_siblings
    }

    /// Get all the right siblngs.
    pub fn right_siblings(&self) -> &Vec<HashValue> {
        &self.right_siblings
    }

    /// Verifies the proof is correct. The verifier needs to have `expected_root_hash`, the index
    /// of the first leaf and all of the leaves in possession.
    pub fn verify(
        &self,
        expected_root_hash: HashValue,
        first_leaf_index: Option<u64>,
        leaf_hashes: &[HashValue],
    ) -> Result<()> {
        if first_leaf_index.is_none() {
            ensure!(
                leaf_hashes.is_empty(),
                "first_leaf_index indicated empty list while leaf_hashes is not empty.",
            );
            ensure!(
                self.left_siblings.is_empty() && self.right_siblings.is_empty(),
                "No siblings are needed.",
            );
            return Ok(());
        }

        ensure!(
            self.left_siblings.len() <= MAX_ACCUMULATOR_PROOF_DEPTH,
            "Proof has more than {} ({}) left siblings.",
            MAX_ACCUMULATOR_PROOF_DEPTH,
            self.left_siblings.len(),
        );
        ensure!(
            self.right_siblings.len() <= MAX_ACCUMULATOR_PROOF_DEPTH,
            "Proof has more than {} ({}) right siblings.",
            MAX_ACCUMULATOR_PROOF_DEPTH,
            self.right_siblings.len(),
        );
        ensure!(
            !leaf_hashes.is_empty(),
            "leaf_hashes is empty while first_leaf_index indicated non-empty list.",
        );

        let mut left_sibling_iter = self.left_siblings.iter().peekable();
        let mut right_sibling_iter = self.right_siblings.iter().peekable();

        let mut first_pos = Position::from_leaf_index(
            first_leaf_index.expect("first_leaf_index should not be None."),
        );
        let mut current_hashes = leaf_hashes.to_vec();
        let mut parent_hashes = vec![];

        // Keep reducing the list of hashes by combining all the children pairs, until there is
        // only one hash left.
        while current_hashes.len() > 1
            || left_sibling_iter.peek().is_some()
            || right_sibling_iter.peek().is_some()
        {
            let mut children_iter = current_hashes.iter();

            // If the first position on the current level is a right child, it needs to be combined
            // with a sibling on the left.
            if first_pos.is_right_child() {
                let left_hash = *left_sibling_iter.next().ok_or_else(|| {
                    format_err!("First child is a right child, but missing sibling on the left.")
                })?;
                let right_hash = *children_iter.next().expect("The first leaf must exist.");
                parent_hashes.push(MerkleTreeInternalNode::<H>::new(left_hash, right_hash).hash());
            }

            // Next we take two children at a time and compute their parents.
            let mut children_iter = children_iter.as_slice().chunks_exact(2);
            while let Some(chunk) = children_iter.next() {
                let left_hash = chunk[0];
                let right_hash = chunk[1];
                parent_hashes.push(MerkleTreeInternalNode::<H>::new(left_hash, right_hash).hash());
            }

            // Similarly, if the last position is a left child, it needs to be combined with a
            // sibling on the right.
            let remainder = children_iter.remainder();
            assert!(remainder.len() <= 1);
            if !remainder.is_empty() {
                let left_hash = remainder[0];
                let right_hash = *right_sibling_iter.next().ok_or_else(|| {
                    format_err!("Last child is a left child, but missing sibling on the right.")
                })?;
                parent_hashes.push(MerkleTreeInternalNode::<H>::new(left_hash, right_hash).hash());
            }

            first_pos = first_pos.parent();
            current_hashes.clear();
            std::mem::swap(&mut current_hashes, &mut parent_hashes);
        }

        ensure!(
            current_hashes[0] == expected_root_hash,
            "Root hashes do not match. Actual root hash: {:x}. Expected root hash: {:x}.",
            current_hashes[0],
            expected_root_hash,
        );

        Ok(())
    }
}

impl<H> std::fmt::Debug for AccumulatorRangeProof<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AccumulatorRangeProof {{ left_siblings: {:?}, right_siblings: {:?} }}",
            self.left_siblings, self.right_siblings,
        )
    }
}

impl<H> PartialEq for AccumulatorRangeProof<H> {
    fn eq(&self, other: &Self) -> bool {
        self.left_siblings == other.left_siblings && self.right_siblings == other.right_siblings
    }
}

impl<H> Eq for AccumulatorRangeProof<H> {}

pub type TransactionAccumulatorRangeProof = AccumulatorRangeProof<TransactionAccumulatorHasher>;
#[cfg(any(test, feature = "fuzzing"))]
pub type TestAccumulatorRangeProof = AccumulatorRangeProof<TestOnlyHasher>;

/// Note: this is not a range proof in the sense that a range of nodes is verified!
/// Instead, it verifies the entire left part of the tree up to a known rightmost node.
/// See the description below.
///
/// A proof that can be used to authenticate a range of consecutive leaves, from the leftmost leaf to
/// the rightmost known one, in a sparse Merkle tree. For example, given the following sparse Merkle tree:
///
/// ```text
///                   root
///                  /     \
///                 /       \
///                /         \
///               o           o
///              / \         / \
///             a   o       o   h
///                / \     / \
///               o   d   e   X
///              / \         / \
///             b   c       f   g
/// ```
///
/// if the proof wants show that `[a, b, c, d, e]` exists in the tree, it would need the siblings
/// `X` and `h` on the right.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SparseMerkleRangeProof {
    /// The vector of siblings on the right of the path from root to last leaf. The ones near the
    /// bottom are at the beginning of the vector. In the above example, it's `[X, h]`.
    right_siblings: Vec<HashValue>,
}

impl SparseMerkleRangeProof {
    /// Constructs a new `SparseMerkleRangeProof`.
    pub fn new(right_siblings: Vec<HashValue>) -> Self {
        Self { right_siblings }
    }

    /// Returns the right siblings.
    pub fn right_siblings(&self) -> &[HashValue] {
        &self.right_siblings
    }

    /// Verifies that the rightmost known leaf exists in the tree and that the resulting
    /// root hash matches the expected root hash.
    pub fn verify(
        &self,
        expected_root_hash: HashValue,
        rightmost_known_leaf: SparseMerkleLeafNode,
        left_siblings: Vec<HashValue>,
    ) -> Result<()> {
        let num_siblings = left_siblings.len() + self.right_siblings.len();
        let mut left_sibling_iter = left_siblings.iter();
        let mut right_sibling_iter = self.right_siblings().iter();

        let mut current_hash = rightmost_known_leaf.hash();
        for bit in rightmost_known_leaf
            .key()
            .iter_bits()
            .rev()
            .skip(HashValue::LENGTH_IN_BITS - num_siblings)
        {
            let (left_hash, right_hash) = if bit {
                (
                    *left_sibling_iter
                        .next()
                        .ok_or_else(|| format_err!("Missing left sibling."))?,
                    current_hash,
                )
            } else {
                (
                    current_hash,
                    *right_sibling_iter
                        .next()
                        .ok_or_else(|| format_err!("Missing right sibling."))?,
                )
            };
            current_hash = SparseMerkleInternalNode::new(left_hash, right_hash).hash();
        }

        ensure!(
            current_hash == expected_root_hash,
            "Root hashes do not match. Actual root hash: {:x}. Expected root hash: {:x}.",
            current_hash,
            expected_root_hash,
        );

        Ok(())
    }
}

/// `TransactionInfo` and a `TransactionAccumulatorProof` connecting it to the ledger root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct TransactionInfoWithProof {
    /// The accumulator proof from ledger info root to leaf that authenticates the hash of the
    /// `TransactionInfo` object.
    pub ledger_info_to_transaction_info_proof: TransactionAccumulatorProof,

    /// The `TransactionInfo` object at the leaf of the accumulator.
    pub transaction_info: TransactionInfo,
}

impl TransactionInfoWithProof {
    /// Constructs a new `TransactionWithProof` object using given
    /// `ledger_info_to_transaction_info_proof`.
    pub fn new(
        ledger_info_to_transaction_info_proof: TransactionAccumulatorProof,
        transaction_info: TransactionInfo,
    ) -> Self {
        Self {
            ledger_info_to_transaction_info_proof,
            transaction_info,
        }
    }

    /// Returns the `ledger_info_to_transaction_info_proof` object in this proof.
    pub fn ledger_info_to_transaction_info_proof(&self) -> &TransactionAccumulatorProof {
        &self.ledger_info_to_transaction_info_proof
    }

    /// Returns the `transaction_info` object in this proof.
    pub fn transaction_info(&self) -> &TransactionInfo {
        &self.transaction_info
    }

    /// Verifies that the `TransactionInfo` exists in the ledger represented by the `LedgerInfo`
    /// at specified version.
    pub fn verify(&self, ledger_info: &LedgerInfo, transaction_version: Version) -> Result<()> {
        verify_transaction_info(
            ledger_info,
            transaction_version,
            &self.transaction_info,
            &self.ledger_info_to_transaction_info_proof,
        )?;
        Ok(())
    }
}

/// The complete proof used to authenticate the state of a resource in state store.
/// This structure consists of the `AccumulatorProof` from `LedgerInfo` to `TransactionInfo`,
/// the `TransactionInfo` object and the `SparseMerkleProof` from state root to the resource.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct StateStoreValueProof {
    transaction_info_with_proof: TransactionInfoWithProof,

    /// The sparse merkle proof from state root to the account state.
    transaction_info_to_value_proof: SparseMerkleProof,
}

impl StateStoreValueProof {
    /// Constructs a new `AccountStateProof` using given `ledger_info_to_transaction_info_proof`,
    /// `transaction_info` and `transaction_info_to_account_proof`.
    pub fn new(
        transaction_info_with_proof: TransactionInfoWithProof,
        transaction_info_to_value_proof: SparseMerkleProof,
    ) -> Self {
        StateStoreValueProof {
            transaction_info_with_proof,
            transaction_info_to_value_proof,
        }
    }

    /// Returns the `transaction_info_with_proof` object in this proof.
    pub fn transaction_info_with_proof(&self) -> &TransactionInfoWithProof {
        &self.transaction_info_with_proof
    }

    /// Returns the `transaction_info_to_account_proof` object in this proof.
    pub fn transaction_info_to_account_proof(&self) -> &SparseMerkleProof {
        &self.transaction_info_to_value_proof
    }

    /// Verifies that the state of an account at version `state_version` is correct using the
    /// provided proof. If `state_value` is present, we expect the account to exist,
    /// otherwise we expect the account to not exist.
    pub fn verify(
        &self,
        ledger_info: &LedgerInfo,
        state_version: Version,
        value_hash: HashValue,
        state_value: Option<&StateValue>,
    ) -> Result<()> {
        self.transaction_info_to_value_proof.verify(
            self.transaction_info_with_proof
                .transaction_info
                .ensure_state_checkpoint_hash()?,
            value_hash,
            state_value,
        )?;

        self.transaction_info_with_proof
            .verify(ledger_info, state_version)?;

        Ok(())
    }
}

/// The complete proof used to authenticate a contract event. This structure consists of the
/// `AccumulatorProof` from `LedgerInfo` to `TransactionInfo`, the `TransactionInfo` object and the
/// `AccumulatorProof` from event accumulator root to the event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct EventProof {
    transaction_info_with_proof: TransactionInfoWithProof,

    /// The accumulator proof from event root to the actual event.
    transaction_info_to_event_proof: EventAccumulatorProof,
}

impl EventProof {
    /// Constructs a new `EventProof` using given `ledger_info_to_transaction_info_proof`,
    /// `transaction_info` and `transaction_info_to_event_proof`.
    pub fn new(
        transaction_info_with_proof: TransactionInfoWithProof,
        transaction_info_to_event_proof: EventAccumulatorProof,
    ) -> Self {
        EventProof {
            transaction_info_with_proof,
            transaction_info_to_event_proof,
        }
    }

    /// Returns the `transaction_info_with_proof` object in this proof.
    pub fn transaction_info_with_proof(&self) -> &TransactionInfoWithProof {
        &self.transaction_info_with_proof
    }

    /// Verifies that a given event is correct using provided proof.
    pub fn verify(
        &self,
        ledger_info: &LedgerInfo,
        event_hash: HashValue,
        transaction_version: Version,
        event_version_within_transaction: Version,
    ) -> Result<()> {
        self.transaction_info_to_event_proof.verify(
            self.transaction_info_with_proof
                .transaction_info()
                .event_root_hash(),
            event_hash,
            event_version_within_transaction,
        )?;

        self.transaction_info_with_proof
            .verify(ledger_info, transaction_version)?;

        Ok(())
    }
}

/// The proof used to authenticate a list of consecutive transaction infos.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct TransactionInfoListWithProof {
    pub ledger_info_to_transaction_infos_proof: TransactionAccumulatorRangeProof,
    pub transaction_infos: Vec<TransactionInfo>,
}

impl TransactionInfoListWithProof {
    pub fn new(
        ledger_info_to_transaction_infos_proof: TransactionAccumulatorRangeProof,
        transaction_infos: Vec<TransactionInfo>,
    ) -> Self {
        Self {
            ledger_info_to_transaction_infos_proof,
            transaction_infos,
        }
    }

    /// Constructs a proof for an empty list of transaction infos. Mostly used for tests.
    pub fn new_empty() -> Self {
        Self::new(AccumulatorRangeProof::new_empty(), vec![])
    }

    /// Verifies the list of transaction infos are correct using the proof. The verifier
    /// needs to have the ledger info and the version of the first transaction in possession.
    pub fn verify(
        &self,
        ledger_info: &LedgerInfo,
        first_transaction_info_version: Option<Version>,
    ) -> Result<()> {
        let txn_info_hashes: Vec<_> = self
            .transaction_infos
            .iter()
            .map(CryptoHash::hash)
            .collect();
        self.ledger_info_to_transaction_infos_proof.verify(
            ledger_info.transaction_accumulator_hash(),
            first_transaction_info_version,
            &txn_info_hashes,
        )
    }

    pub fn verify_extends_ledger(
        &self,
        num_txns_in_ledger: LeafCount,
        root_hash: HashValue,
        first_transaction_info_version: Option<Version>,
    ) -> Result<usize> {
        if let Some(first_version) = first_transaction_info_version {
            ensure!(
                first_version <= num_txns_in_ledger,
                "Transaction list too new. Expected version: {}. First transaction version: {}.",
                num_txns_in_ledger,
                first_version
            );
            let num_overlap_txns = (num_txns_in_ledger - first_version) as usize;
            if num_overlap_txns > self.transaction_infos.len() {
                // Entire chunk is in the past, hard to verify if there's a fork.
                // A fork will need to be detected later.
                return Ok(self.transaction_infos.len());
            }
            let overlap_txn_infos = &self.transaction_infos[..num_overlap_txns];

            // Left side of the proof happens to be the frozen subtree roots of the accumulator
            // right before the list of txns are applied.
            let frozen_subtree_roots_from_proof = self
                .ledger_info_to_transaction_infos_proof
                .left_siblings()
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>();
            let accu_from_proof = InMemoryAccumulator::<TransactionAccumulatorHasher>::new(
                frozen_subtree_roots_from_proof,
                first_version,
            )?
            .append(
                &overlap_txn_infos
                    .iter()
                    .map(CryptoHash::hash)
                    .collect::<Vec<_>>()[..],
            );
            // The two accumulator root hashes should be identical.
            ensure!(
                accu_from_proof.root_hash() == root_hash,
                "Fork happens because the current synced_trees doesn't match the txn list provided."
            );
            Ok(num_overlap_txns)
        } else {
            // Assuming input is empty
            ensure!(self.transaction_infos.is_empty());
            Ok(0)
        }
    }
}

/// A proof that first verifies that establishes correct computation of the root and then
/// returns the new tree to acquire a new root and version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AccumulatorExtensionProof<H> {
    /// Represents the roots of all the full subtrees from left to right in the original accumulator.
    frozen_subtree_roots: Vec<HashValue>,
    /// The total number of leaves in original accumulator.
    num_leaves: LeafCount,
    /// The values representing the newly appended leaves.
    leaves: Vec<HashValue>,

    hasher: PhantomData<H>,
}

impl<H: CryptoHasher> AccumulatorExtensionProof<H> {
    pub fn new(
        frozen_subtree_roots: Vec<HashValue>,
        num_leaves: LeafCount,
        leaves: Vec<HashValue>,
    ) -> Self {
        Self {
            frozen_subtree_roots,
            num_leaves,
            leaves,
            hasher: PhantomData,
        }
    }

    pub fn verify(&self, original_root: HashValue) -> anyhow::Result<InMemoryAccumulator<H>> {
        let original_tree =
            InMemoryAccumulator::<H>::new(self.frozen_subtree_roots.clone(), self.num_leaves)?;
        ensure!(
            original_tree.root_hash() == original_root,
            "Root hashes do not match. Actual root hash: {:x}. Expected root hash: {:x}.",
            original_tree.root_hash(),
            original_root
        );

        Ok(original_tree.append(self.leaves.as_slice()))
    }
}
