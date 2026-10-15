//! Binary Merkle trees over 32-byte leaves.
//!
//! Leaves and interior nodes are hashed in different domains, which blocks
//! the classic second-preimage attack of passing an interior node off as a
//! leaf. An odd node at the end of a level is promoted unchanged instead of
//! being paired with a copy of itself, so two different leaf lists can never
//! share a root (the Bitcoin CVE-2012-2459 duplication bug).

use crate::Hash32;
use serde::{Deserialize, Serialize};

fn leaf_hash(leaf: &Hash32) -> Hash32 {
    Hash32::digest("merkle-leaf", leaf.as_bytes())
}

fn node_hash(left: &Hash32, right: &Hash32) -> Hash32 {
    let mut data = [0u8; 64];
    data[..32].copy_from_slice(left.as_bytes());
    data[32..].copy_from_slice(right.as_bytes());
    Hash32::digest("merkle-node", &data)
}

pub fn root(leaves: &[Hash32]) -> Hash32 {
    if leaves.is_empty() {
        return Hash32::digest("merkle-empty", &[]);
    }
    let mut level: Vec<Hash32> = leaves.iter().map(leaf_hash).collect();
    while level.len() > 1 {
        level = next_level(&level);
    }
    level[0]
}

fn next_level(level: &[Hash32]) -> Vec<Hash32> {
    level
        .chunks(2)
        .map(|pair| match pair {
            [left, right] => node_hash(left, right),
            [single] => *single,
            _ => unreachable!("chunks(2) yields one or two items"),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Left,
    Right,
}

/// Inclusion proof: sibling hashes from the leaf up to the root, each tagged
/// with the side the sibling sits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    pub index: u64,
    pub leaf_count: u64,
    pub path: Vec<(Side, Hash32)>,
}

pub fn prove(leaves: &[Hash32], index: usize) -> Option<Proof> {
    if index >= leaves.len() {
        return None;
    }
    let mut level: Vec<Hash32> = leaves.iter().map(leaf_hash).collect();
    let mut position = index;
    let mut path = Vec::new();
    while level.len() > 1 {
        let sibling = position ^ 1;
        if sibling < level.len() {
            let side = if sibling < position {
                Side::Left
            } else {
                Side::Right
            };
            path.push((side, level[sibling]));
        }
        level = next_level(&level);
        position /= 2;
    }
    Some(Proof {
        index: index as u64,
        leaf_count: leaves.len() as u64,
        path,
    })
}

pub fn verify(root_hash: &Hash32, leaf: &Hash32, proof: &Proof) -> bool {
    if proof.index >= proof.leaf_count {
        return false;
    }
    let mut current = leaf_hash(leaf);
    for (side, sibling) in &proof.path {
        current = match side {
            Side::Left => node_hash(sibling, &current),
            Side::Right => node_hash(&current, sibling),
        };
    }
    current == *root_hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<Hash32> {
        (0..n)
            .map(|i| Hash32::digest("test", &(i as u64).to_be_bytes()))
            .collect()
    }

    #[test]
    fn every_leaf_proves_for_many_sizes() {
        for n in 1..=17 {
            let items = leaves(n);
            let r = root(&items);
            for (i, leaf) in items.iter().enumerate() {
                let proof = prove(&items, i).unwrap();
                assert!(verify(&r, leaf, &proof), "n={n} i={i}");
                assert!(!verify(&r, &Hash32::ZERO, &proof));
            }
        }
    }

    #[test]
    fn odd_promotion_prevents_duplicate_root_collision() {
        let three = leaves(3);
        let mut four = three.clone();
        four.push(three[2]);
        assert_ne!(root(&three), root(&four));
    }

    #[test]
    fn leaf_cannot_pose_as_interior_node() {
        let items = leaves(2);
        let interior = node_hash(&leaf_hash(&items[0]), &leaf_hash(&items[1]));
        assert_ne!(root(&[interior]), root(&items));
    }
}
