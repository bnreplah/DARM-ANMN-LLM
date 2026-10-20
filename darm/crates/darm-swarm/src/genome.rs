//! Node genomes and governed replication.
//!
//! A genome is the blueprint a new node is configured from: its role,
//! specializations and tuning. Offspring are derived deterministically from
//! `(parent genome, seed)` so validators can recompute exactly what they are
//! voting on. The replication policy only *recommends*; the recommendation
//! becomes a ledger `Proposal { kind: "replicate" }` and infrastructure acts
//! only on proposals that passed the validator vote.

use crate::lifecycle::{Lifecycle, NodeState, Role};
use crate::rng::SplitMix64;
use darm_core::Hash32;
use darm_core::canonical::Canonical;
use serde::{Deserialize, Serialize};

pub const SPECIALIZATION_POOL: &[&str] = &[
    "programming",
    "analysis",
    "mathematics",
    "optimization",
    "security",
    "retrieval",
    "summarization",
    "planning",
    "vision",
    "translation",
];

const MAX_SPECIALIZATIONS: usize = 6;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Genome {
    pub role: Role,
    pub specializations: Vec<String>,
    /// Probability of each mutation, 0..=0.5.
    pub mutation_rate: f64,
    /// Multiplier on energy cost, lower is more efficient (0.5..=1.5).
    pub energy_efficiency: f64,
    pub generation: u32,
    pub parent: Option<Hash32>,
}

impl Genome {
    pub fn root(role: Role, specializations: Vec<String>) -> Self {
        Self {
            role,
            specializations,
            mutation_rate: 0.15,
            energy_efficiency: 1.0,
            generation: 0,
            parent: None,
        }
    }

    pub fn commitment(&self) -> Hash32 {
        let mut encoder = Canonical::new("genome");
        encoder
            .str(self.role.as_str())
            .str_list(&self.specializations)
            .u32((self.mutation_rate * 1_000_000.0).round() as u32)
            .u32((self.energy_efficiency * 1_000_000.0).round() as u32)
            .u32(self.generation)
            .opt_fixed32(self.parent.as_ref().map(Hash32::as_bytes));
        Hash32::digest("genome", encoder.as_bytes())
    }

    /// Deterministic offspring: same parent and seed, same child.
    pub fn offspring(&self, seed: u64) -> Self {
        let mut rng = SplitMix64::new(
            seed ^ u64::from_be_bytes(
                self.commitment().as_bytes()[..8]
                    .try_into()
                    .expect("8 bytes"),
            ),
        );
        let mut child = self.clone();
        child.generation = self.generation.saturating_add(1);
        child.parent = Some(self.commitment());
        if rng.next_f64() < self.mutation_rate && child.specializations.len() < MAX_SPECIALIZATIONS
        {
            let candidate = SPECIALIZATION_POOL[rng.below(SPECIALIZATION_POOL.len())];
            if !child.specializations.iter().any(|s| s == candidate) {
                child.specializations.push(candidate.to_string());
            }
        }
        if rng.next_f64() < self.mutation_rate {
            let delta = (rng.next_f64() - 0.6) * 0.2; // biased toward efficiency
            child.energy_efficiency = (child.energy_efficiency + delta).clamp(0.5, 1.5);
        }
        if rng.next_f64() < self.mutation_rate {
            let delta = (rng.next_f64() - 0.5) * 0.05;
            child.mutation_rate = (child.mutation_rate + delta).clamp(0.01, 0.5);
        }
        child
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplicationPolicy {
    pub min_health: f64,
    pub min_energy: f64,
    pub min_reputation: f64,
    pub min_age_ms: u64,
    pub min_population: usize,
    pub max_population: usize,
}

impl Default for ReplicationPolicy {
    /// The PoC's criteria: health > 90, energy > 80, reputation > 2.5,
    /// age > 5 minutes, population within 10..=1000.
    fn default() -> Self {
        Self {
            min_health: 90.0,
            min_energy: 80.0,
            min_reputation: 2.5,
            min_age_ms: 5 * 60 * 1_000,
            min_population: 10,
            max_population: 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicationRecommendation {
    pub parent: Hash32,
    pub seed: u64,
    pub offspring: Genome,
    pub reason: String,
}

impl ReplicationRecommendation {
    /// Hash used as the ledger proposal payload.
    pub fn payload_hash(&self) -> Hash32 {
        let mut encoder = Canonical::new("replication-proposal");
        encoder
            .fixed32(self.parent.as_bytes())
            .u64(self.seed)
            .fixed32(self.offspring.commitment().as_bytes());
        Hash32::digest("replication-proposal", encoder.as_bytes())
    }
}

impl ReplicationPolicy {
    pub fn evaluate(
        &self,
        lifecycle: &Lifecycle,
        genome: &Genome,
        population: usize,
        now_ms: u64,
        seed: u64,
    ) -> Option<ReplicationRecommendation> {
        if population >= self.max_population {
            return None;
        }
        // Below the floor the swarm replicates to recover, regardless of the
        // thriving criteria (the PoC's "emergency replication"), but a
        // damaged node never reproduces.
        let emergency = population < self.min_population
            && !matches!(lifecycle.state, NodeState::Damaged | NodeState::Healing);
        let vitals = lifecycle.vitals;
        let thriving = vitals.health > self.min_health
            && vitals.energy > self.min_energy
            && vitals.reputation > self.min_reputation
            && lifecycle.age_ms(now_ms) > self.min_age_ms;
        if !(emergency || thriving) {
            return None;
        }
        Some(ReplicationRecommendation {
            parent: genome.commitment(),
            seed,
            offspring: genome.offspring(seed),
            reason: if emergency {
                format!(
                    "population {population} below floor {}",
                    self.min_population
                )
            } else {
                "parent is thriving".to_string()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offspring_is_deterministic_and_increments_generation() {
        let parent = Genome::root(Role::Worker, vec!["programming".into()]);
        let a = parent.offspring(42);
        let b = parent.offspring(42);
        assert_eq!(a, b);
        assert_eq!(a.generation, 1);
        assert_eq!(a.parent, Some(parent.commitment()));
        assert_ne!(a.commitment(), parent.commitment());
    }

    #[test]
    fn mutations_stay_in_bounds_over_many_generations() {
        let mut genome = Genome::root(Role::Scout, vec![]);
        genome.mutation_rate = 0.5;
        for seed in 0..500 {
            genome = genome.offspring(seed);
            assert!((0.5..=1.5).contains(&genome.energy_efficiency));
            assert!((0.01..=0.5).contains(&genome.mutation_rate));
            assert!(genome.specializations.len() <= MAX_SPECIALIZATIONS);
        }
        assert_eq!(genome.generation, 500);
    }

    #[test]
    fn policy_matches_poc_criteria() {
        let policy = ReplicationPolicy::default();
        let genome = Genome::root(Role::Worker, vec![]);
        let mut life = Lifecycle::new(Role::Worker, 0);
        life.state = NodeState::Flowering;
        life.vitals.reputation = 3.0;
        let old_enough = 6 * 60 * 1_000;
        assert!(policy.evaluate(&life, &genome, 50, old_enough, 1).is_some());
        assert!(
            policy.evaluate(&life, &genome, 50, 60_000, 1).is_none(),
            "too young"
        );
        assert!(
            policy
                .evaluate(&life, &genome, 1_000, old_enough, 1)
                .is_none(),
            "full"
        );
        life.vitals.reputation = 1.0;
        assert!(policy.evaluate(&life, &genome, 50, old_enough, 1).is_none());
        assert!(
            policy.evaluate(&life, &genome, 3, old_enough, 1).is_some(),
            "emergency"
        );
        life.state = NodeState::Damaged;
        assert!(policy.evaluate(&life, &genome, 3, old_enough, 1).is_none());
    }
}
