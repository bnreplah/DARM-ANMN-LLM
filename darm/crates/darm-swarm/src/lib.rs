//! DARM-ANN biomimetic swarm layer.
//!
//! The original proof of concept modelled nodes as bees and ants: a
//! lifecycle with health and energy, pheromone trails that decay, a genome
//! that mutates across generations, and self-replication. This crate keeps
//! those concepts but grounds every signal in something the mesh can measure:
//!
//! - **energy** tracks spare capacity (in-flight load vs. capacity),
//! - **health** tracks request success and latency,
//! - **pheromones** are deposited on concrete `(node, model)` routes when a
//!   request succeeds (and "danger" when it fails), and they steer routing
//!   with ant-colony-optimization weighting,
//! - **replication** produces a *recommendation* that must pass a ledger
//!   proposal vote before any infrastructure acts on it. Nothing in this
//!   crate spawns processes: an autonomous self-replicating workload with
//!   cluster credentials is a worm, and DARM-ANN keeps a human-governed
//!   quorum in that loop.

pub mod genome;
pub mod lifecycle;
pub mod pheromone;
pub mod rng;

pub use genome::{Genome, ReplicationPolicy, ReplicationRecommendation};
pub use lifecycle::{Lifecycle, NodeState, Role, Signals, Vitals};
pub use pheromone::{PheromoneField, PheromoneKind, RouteKey};
