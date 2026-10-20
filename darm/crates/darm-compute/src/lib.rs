//! DARM-ANN compute pool: CPUs, GPUs and phones working as one.
//!
//! mesh-llm pools GPU memory to serve models. DARM-ANN adds a pool for
//! *compute work* that treats every processor the same way: a server CPU, a
//! discrete GPU, an Apple unified-memory SoC and a phone on a charger are all
//! [`device::Device`]s with a measured throughput, and jobs can ask for CPU
//! only, GPU only, either, or a split across both.
//!
//! - [`device`]: device profiles, live conditions (battery, charging,
//!   metered network, thermal) and the owner's contribution policy. Mobile
//!   devices contribute only when the owner's policy allows it (by default:
//!   charging, unmetered, not hot).
//! - [`advert`]: signed, short-lived capability advertisements gossiped to
//!   the mesh, with replay protection by sequence number.
//! - [`pool`]: the registry of live adverts and the heterogeneous scheduler.
//!   Shards are dealt by smooth weighted round robin over effective
//!   throughput (advertised, corrected by measurement) × reliability × trust,
//!   with replicas of a shard always on distinct nodes.
//! - [`receipt`]: workers sign what they computed; the coordinator tallies
//!   replicas by majority, flags dissenters, and produces a ledger
//!   `ComputeCommit` (proof of useful work) instead of burning hashes.
//! - [`sizing`]: adaptive shard sizing so a phone gets small shards and a GPU
//!   large ones, converging on a target shard duration.
//!
//! No remote code is ever executed by this crate. Workloads are named kinds
//! that each node's operator chose to serve; the pool only plans, measures
//! and verifies.

pub mod admission;
pub mod advert;
pub mod device;
pub mod pool;
pub mod receipt;
pub mod sizing;

pub use admission::{ModeledAdmission, Threshold, admission_probability};
pub use advert::{CapabilityAdvert, SignedAdvert};
pub use device::{
    Backend, ComputeProfile, Conditions, ContributionPolicy, Device, DeviceKind, FormFactor,
    OfferedDevice, Thermal, WorkloadKind,
};
pub use pool::{
    Assignment, ComputePool, DevicePreference, JobPlan, JobSpec, PlanContext, PoolSummary,
    SyncClass,
};
pub use receipt::{ShardVerdict, SignedReceipt, Tally, WorkReceipt, tally};
pub use sizing::ShardSizer;
