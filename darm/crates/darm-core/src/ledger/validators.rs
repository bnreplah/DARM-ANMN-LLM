use crate::canonical::Canonical;
use crate::{DarmError, Hash32, NodeId, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Weighted validator set. Weights are integers so that quorum arithmetic is
/// exact and identical on every implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ValidatorSet {
    members: BTreeMap<NodeId, u64>,
}

impl ValidatorSet {
    pub fn new(members: impl IntoIterator<Item = (NodeId, u64)>) -> Result<Self> {
        let mut map = BTreeMap::new();
        for (node, weight) in members {
            if weight == 0 {
                return Err(DarmError::Rejected(format!(
                    "validator {} has zero weight",
                    node.short()
                )));
            }
            if map.insert(node, weight).is_some() {
                return Err(DarmError::Rejected(format!(
                    "validator {} listed twice",
                    node.short()
                )));
            }
        }
        if map.is_empty() {
            return Err(DarmError::Rejected("validator set is empty".into()));
        }
        let set = Self { members: map };
        // Guard the 3*sum arithmetic in `has_quorum` against overflow.
        set.members
            .values()
            .try_fold(0u64, |acc, w| acc.checked_add(*w))
            .and_then(|total| total.checked_mul(3))
            .ok_or_else(|| DarmError::Rejected("validator weights overflow".into()))?;
        Ok(set)
    }

    /// Equal-weight set, the common case for a private mesh.
    pub fn equal(members: impl IntoIterator<Item = NodeId>) -> Result<Self> {
        Self::new(members.into_iter().map(|node| (node, 1)))
    }

    pub fn contains(&self, node: &NodeId) -> bool {
        self.members.contains_key(node)
    }

    pub fn weight(&self, node: &NodeId) -> u64 {
        self.members.get(node).copied().unwrap_or(0)
    }

    pub fn total_weight(&self) -> u64 {
        self.members.values().sum()
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn members(&self) -> impl Iterator<Item = (&NodeId, &u64)> {
        self.members.iter()
    }

    /// Strictly more than two thirds of the total weight.
    pub fn has_quorum<'a>(&self, signers: impl IntoIterator<Item = &'a NodeId>) -> bool {
        let unique: BTreeSet<&NodeId> = signers.into_iter().collect();
        let signed: u64 = unique.into_iter().map(|node| self.weight(node)).sum();
        signed * 3 > self.total_weight() * 2
    }

    /// Round-robin proposer for `(height, round)`.
    pub fn proposer_for(&self, height: u64, round: u32) -> NodeId {
        let index = (height.wrapping_add(u64::from(round))) % self.members.len() as u64;
        *self
            .members
            .keys()
            .nth(index as usize)
            .expect("index is reduced modulo a non-empty set")
    }

    pub fn commitment(&self) -> Hash32 {
        let mut encoder = Canonical::new("validator-set");
        encoder.u32(self.members.len() as u32);
        for (node, weight) in &self.members {
            encoder.fixed32(node.as_bytes()).u64(*weight);
        }
        Hash32::digest("validator-set", encoder.as_bytes())
    }
}

/// Static chain parameters every participant must share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainConfig {
    /// Human-readable chain name, e.g. the mesh id. Hashed into every
    /// signature so entries cannot be replayed on another chain.
    pub name: String,
    pub validators: ValidatorSet,
}

impl ChainConfig {
    pub fn chain_id(&self) -> Hash32 {
        Hash32::digest("chain-id", self.name.as_bytes())
    }

    /// Hash that block 1 links to.
    pub fn genesis_hash(&self) -> Hash32 {
        let mut encoder = Canonical::new("genesis");
        encoder
            .fixed32(self.chain_id().as_bytes())
            .fixed32(self.validators.commitment().as_bytes());
        Hash32::digest("genesis", encoder.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeIdentity;

    fn node(seed: u8) -> NodeId {
        NodeIdentity::from_secret_bytes(&[seed; 32]).node_id()
    }

    #[test]
    fn quorum_is_strictly_more_than_two_thirds() {
        let set = ValidatorSet::equal([node(1), node(2), node(3)]).unwrap();
        assert!(!set.has_quorum([&node(1), &node(2)]), "2/3 is not enough");
        assert!(set.has_quorum([&node(1), &node(2), &node(3)]));
        let four = ValidatorSet::equal([node(1), node(2), node(3), node(4)]).unwrap();
        assert!(four.has_quorum([&node(1), &node(2), &node(3)]));
        assert!(
            !four.has_quorum([&node(1), &node(2), &node(9)]),
            "outsiders weigh nothing"
        );
    }

    #[test]
    fn duplicate_signers_do_not_double_count() {
        let set = ValidatorSet::equal([node(1), node(2), node(3)]).unwrap();
        assert!(!set.has_quorum([&node(1), &node(1), &node(1)]));
    }

    #[test]
    fn rejects_bad_sets() {
        assert!(ValidatorSet::equal([]).is_err());
        assert!(ValidatorSet::new([(node(1), 0)]).is_err());
        assert!(ValidatorSet::new([(node(1), u64::MAX), (node(2), 1)]).is_err());
    }

    #[test]
    fn proposer_rotates() {
        let set = ValidatorSet::equal([node(1), node(2), node(3)]).unwrap();
        let a = set.proposer_for(1, 0);
        let b = set.proposer_for(1, 1);
        assert_ne!(a, b);
        assert_eq!(set.proposer_for(2, 0), b);
    }
}
