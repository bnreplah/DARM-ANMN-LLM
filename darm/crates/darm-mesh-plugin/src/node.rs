//! One DARM-ANN node: identity, ledger replica, memory store, pheromone
//! field and lifecycle. Pure state machine: every method takes `now_ms` and
//! returns the gossip it wants sent, so it can be tested without a mesh.

use crate::config::DarmConfig;
use crate::wire::{MAX_BLOCKS_PER_SYNC, Outbound, Wire};
use darm_core::ledger::{
    ChainConfig, EntryBody, Ledger, ProposalOutcome, SignedEntry, ValidatorSet, store::BlockLog,
};
use darm_core::{DarmError, Hash32, NodeId, NodeIdentity, Result};
use darm_memory::{
    Hit, MemoryKind, MemoryLimits, MemoryLog, MemoryRecord, MemoryStore, Origin, Scope,
    SearchOptions, SignedMemory,
};
use darm_swarm::{
    Genome, Lifecycle, PheromoneField, ReplicationPolicy, ReplicationRecommendation, Role,
    RouteKey, Signals,
};
use serde_json::{Value, json};
use std::collections::HashMap;

const ANNOUNCE_INTERVAL_MS: u64 = 30_000;
const ROUND_TIMEOUT_MS: u64 = 10_000;
const MEMORIES_PER_PUSH: usize = 24;
const MAX_REQUEST_IDS: usize = 64;
const REPLICATION_COOLDOWN_MS: u64 = 60 * 60 * 1_000;
const REPLICATION_VOTING_MS: u64 = 60 * 60 * 1_000;
/// Per-peer token bucket for inbound gossip.
const BUCKET_CAPACITY: f64 = 200.0;
const BUCKET_REFILL_PER_SEC: f64 = 50.0;
const MAX_TRACKED_PEERS: usize = 4_096;

/// One periodic duty of [`DarmNode::tick`].
type TickStep = fn(&mut DarmNode, u64) -> Result<Vec<Outbound>>;

#[derive(Debug, Default, Clone, Copy)]
struct Window {
    successes: u32,
    failures: u32,
    latency_sum_ms: f64,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated_ms: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Counters {
    pub gossip_received: u64,
    pub gossip_rejected: u64,
    pub gossip_rate_limited: u64,
    pub memories_from_peers: u64,
    pub turns: u64,
    pub turns_failed: u64,
}

pub struct DarmNode {
    pub config: DarmConfig,
    identity: NodeIdentity,
    ledger: Ledger,
    block_log: Option<BlockLog>,
    persisted_height: u64,
    memory: MemoryStore,
    pheromones: PheromoneField,
    lifecycle: Lifecycle,
    genome: Genome,
    public_mesh: bool,
    round: u32,
    proposed_at: Option<(u64, u32)>,
    last_progress_ms: u64,
    last_announce_ms: u64,
    window: Window,
    in_flight: u32,
    buckets: HashMap<String, Bucket>,
    registration_submitted: bool,
    last_replication_ms: u64,
    replication: Vec<(Hash32, ReplicationRecommendation)>,
    archived: std::collections::HashSet<Hash32>,
    pub counters: Counters,
}

impl DarmNode {
    /// Open (or create) the node's persistent state under the configured
    /// data directory. Without one, state is in-memory only.
    pub fn open(config: DarmConfig, now_ms: u64) -> Result<Self> {
        let Some(dir) = config.data_dir.clone() else {
            return Self::ephemeral(config, NodeIdentity::generate()?, now_ms);
        };
        std::fs::create_dir_all(&dir)?;
        restrict_dir(&dir)?;
        let identity = NodeIdentity::load_or_create(&dir.join("identity.key"))?;
        let chain = chain_config(&config, &identity)?;
        let log = BlockLog::new(dir.join(format!("ledger-{}.jsonl", chain.chain_id().short())));
        let ledger = log.load(chain)?;
        let memory = MemoryStore::open(
            MemoryLog::new(dir.join("memory.jsonl")),
            MemoryLimits::default(),
        )?;
        let mut node = Self::assemble(config, identity, ledger, memory, now_ms);
        node.persisted_height = node.ledger.height();
        node.block_log = Some(log);
        Ok(node)
    }

    pub fn ephemeral(config: DarmConfig, identity: NodeIdentity, now_ms: u64) -> Result<Self> {
        let ledger = Ledger::new(chain_config(&config, &identity)?);
        let memory = MemoryStore::new(MemoryLimits::default());
        Ok(Self::assemble(config, identity, ledger, memory, now_ms))
    }

    fn assemble(
        config: DarmConfig,
        identity: NodeIdentity,
        ledger: Ledger,
        memory: MemoryStore,
        now_ms: u64,
    ) -> Self {
        let role = if ledger.is_validator(&identity.node_id()) {
            Role::Guard
        } else {
            Role::Worker
        };
        Self {
            config,
            identity,
            ledger,
            block_log: None,
            persisted_height: 0,
            memory,
            pheromones: PheromoneField::default(),
            lifecycle: Lifecycle::new(role, now_ms),
            genome: Genome::root(role, vec!["retrieval".into()]),
            public_mesh: false,
            round: 0,
            proposed_at: None,
            last_progress_ms: now_ms,
            last_announce_ms: 0,
            window: Window::default(),
            in_flight: 0,
            buckets: HashMap::new(),
            registration_submitted: false,
            last_replication_ms: 0,
            replication: Vec::new(),
            archived: std::collections::HashSet::new(),
            counters: Counters::default(),
        }
    }

    pub fn node_id(&self) -> NodeId {
        self.identity.node_id()
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    pub fn set_public_mesh(&mut self, public: bool) {
        self.public_mesh = public;
    }

    fn mesh_allows_gossip(&self) -> bool {
        !self.public_mesh || self.config.allow_public_gossip
    }

    /// Memory gossip is on only when sharing is enabled.
    pub fn memory_gossip(&self) -> bool {
        self.config.share && self.mesh_allows_gossip()
    }

    /// Ledger gossip is on when the chain has more than one validator.
    pub fn ledger_gossip(&self) -> bool {
        self.ledger.config().validators.len() > 1 && self.mesh_allows_gossip()
    }

    /// How much a memory author is trusted for prompt injection.
    pub fn trust(&self, author: &NodeId) -> f32 {
        if *author == self.node_id() {
            return 1.0;
        }
        if self.config.trusted_authors.contains(author) || self.ledger.is_validator(author) {
            let reputation = self.ledger.state().reputation(author) as f32 / 1_000.0;
            return (0.5 + reputation / 10.0).clamp(0.0, 1.0);
        }
        0.0
    }

    // ----- memory -------------------------------------------------------

    pub fn recall(&mut self, query: &str, namespace: &str, limit: usize, now_ms: u64) -> Vec<Hit> {
        let tag = namespace_tag(namespace);
        let trusted: Vec<(NodeId, f32)> = self
            .config
            .trusted_authors
            .iter()
            .chain(self.ledger.config().validators.members().map(|(n, _)| n))
            .map(|n| (*n, self.trust(n)))
            .chain(std::iter::once((self.node_id(), 1.0)))
            .collect();
        let trust = move |author: &NodeId| {
            trusted
                .iter()
                .find(|(n, _)| n == author)
                .map_or(0.0, |(_, t)| *t)
        };
        let options = SearchOptions {
            limit,
            trust: &trust,
            required_tag: Some(&tag),
            ..SearchOptions::default()
        };
        self.memory.search(query, &options, now_ms)
    }

    /// Write a memory as this node. Mesh-scoped memories are committed to
    /// the ledger and pushed to peers.
    pub fn remember(
        &mut self,
        mut record: MemoryRecord,
        namespace: &str,
        now_ms: u64,
    ) -> Result<(SignedMemory, Vec<Outbound>)> {
        let tag = namespace_tag(namespace);
        if !record.tags.contains(&tag) {
            record.tags.push(tag);
        }
        record.created_ms = now_ms;
        let signed = self.memory.remember(&self.identity, record)?;
        let mut out = Vec::new();
        if signed.record.scope == Scope::Mesh {
            let entry = self.sign_entry(
                EntryBody::MemoryCommit {
                    memory: signed.id(),
                    importance_milli: signed.record.importance_milli,
                },
                now_ms,
            )?;
            self.ledger.submit(entry.clone(), now_ms)?;
            if self.ledger_gossip() {
                out.push(Outbound::broadcast(Wire::LedgerEntry { entry }));
            }
            if self.memory_gossip() {
                out.push(Outbound::broadcast(Wire::MemoryPush {
                    memories: vec![signed.clone()],
                }));
            }
        }
        Ok((signed, out))
    }

    pub fn remember_exchange(
        &mut self,
        user: &str,
        assistant: &str,
        model: &str,
        namespace: &str,
        now_ms: u64,
    ) -> Result<(Hash32, Vec<Outbound>)> {
        let content = format!(
            "User: {}\nAssistant: {}",
            truncate_chars(user, 2_000),
            truncate_chars(assistant, 4_000)
        );
        let mut tags = vec!["chat".to_string()];
        let model_tag = sanitize_tag(&format!("model:{model}"));
        if !model_tag.is_empty() {
            tags.push(model_tag);
        }
        let record = MemoryRecord {
            author: self.node_id(),
            created_ms: now_ms,
            kind: MemoryKind::Episodic,
            scope: if self.config.share {
                Scope::Mesh
            } else {
                Scope::Local
            },
            importance_milli: 300,
            valence_milli: 0,
            tags,
            content,
            parent: None,
        };
        let (signed, out) = self.remember(record, namespace, now_ms)?;
        Ok((signed.id(), out))
    }

    pub fn forget(&mut self, id: &Hash32, now_ms: u64) -> Result<Vec<Outbound>> {
        let was_committed = self.ledger.state().is_committed(id);
        let tombstone = self.memory.forget(&self.identity, id, now_ms)?;
        let mut out = Vec::new();
        if self.memory_gossip() {
            out.push(Outbound::broadcast(Wire::Tombstone { tombstone }));
        }
        if was_committed {
            let entry = self.sign_entry(EntryBody::MemoryRevoke { memory: *id }, now_ms)?;
            self.ledger.submit(entry.clone(), now_ms)?;
            if self.ledger_gossip() {
                out.push(Outbound::broadcast(Wire::LedgerEntry { entry }));
            }
        }
        Ok(out)
    }

    // ----- routing ------------------------------------------------------

    /// Order candidate routes by pheromone desirability. `heuristic` > 0.
    pub fn rank_routes(&self, routes: &[(RouteKey, f64)], now_ms: u64, seed: u64) -> Vec<RouteKey> {
        self.pheromones.rank(routes, now_ms, seed)
    }

    pub fn begin_request(&mut self) {
        self.in_flight = self.in_flight.saturating_add(1);
    }

    pub fn end_request(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    pub fn record_outcome(
        &mut self,
        route: &RouteKey,
        success: bool,
        latency_ms: f64,
        now_ms: u64,
    ) {
        self.pheromones
            .record_outcome(route, success, latency_ms, now_ms);
        if success {
            self.window.successes += 1;
            self.window.latency_sum_ms += latency_ms;
        } else {
            self.window.failures += 1;
        }
    }

    // ----- ledger -------------------------------------------------------

    fn sign_entry(&self, body: EntryBody, now_ms: u64) -> Result<SignedEntry> {
        SignedEntry::sign(&self.identity, self.ledger.chain_id(), body, now_ms)
    }

    /// Persist newly finalized blocks and consolidate their memories.
    fn sync_effects(&mut self) -> Result<()> {
        while self.persisted_height < self.ledger.height() {
            let height = self.persisted_height + 1;
            let block = self
                .ledger
                .block(height)
                .expect("height is within the chain")
                .clone();
            if let Some(log) = &self.block_log {
                log.append(&block)?;
            }
            for entry in &block.entries {
                if let EntryBody::MemoryCommit { memory, .. } = &entry.entry.body {
                    self.memory.mark_finalized(memory)?;
                }
            }
            self.persisted_height = height;
            self.round = 0;
            self.proposed_at = None;
            self.last_progress_ms = block.header.timestamp_ms;
        }
        Ok(())
    }

    fn try_propose(&mut self, now_ms: u64) -> Result<Vec<Outbound>> {
        let me = self.node_id();
        let next = self.ledger.height() + 1;
        if self.ledger.pending_len() == 0
            || self.ledger.proposer_for_next(self.round) != me
            || self.proposed_at == Some((next, self.round))
        {
            return Ok(Vec::new());
        }
        self.proposed_at = Some((next, self.round));
        let Some(mut block) = self.ledger.propose(&self.identity, self.round, now_ms)? else {
            return Ok(Vec::new());
        };
        let hash = block.hash();
        if let Ok((attestation, _)) = self.ledger.attest(&self.identity, &hash) {
            block.attestations.push(attestation);
        }
        self.sync_effects()?;
        if self.ledger_gossip() {
            return Ok(vec![Outbound::broadcast(Wire::BlockProposal { block })]);
        }
        Ok(Vec::new())
    }

    fn ensure_registered(&mut self, now_ms: u64) -> Result<Vec<Outbound>> {
        if self.registration_submitted
            || self.ledger.state().registration(&self.node_id()).is_some()
        {
            return Ok(Vec::new());
        }
        self.registration_submitted = true;
        let entry = self.sign_entry(
            EntryBody::NodeRegistration {
                role: self.lifecycle.role.as_str().into(),
                genome: self.genome.commitment(),
                generation: 0,
                parent: None,
            },
            now_ms,
        )?;
        self.ledger.submit(entry.clone(), now_ms)?;
        Ok(if self.ledger_gossip() {
            vec![Outbound::broadcast(Wire::LedgerEntry { entry })]
        } else {
            Vec::new()
        })
    }

    fn consider_replication(&mut self, now_ms: u64) -> Result<Vec<Outbound>> {
        if !self.config.propose_replication
            || now_ms.saturating_sub(self.last_replication_ms) < REPLICATION_COOLDOWN_MS
        {
            return Ok(Vec::new());
        }
        let population = self.ledger.state().registrations().count();
        let Some(recommendation) = ReplicationPolicy::default().evaluate(
            &self.lifecycle,
            &self.genome,
            population,
            now_ms,
            now_ms,
        ) else {
            return Ok(Vec::new());
        };
        self.last_replication_ms = now_ms;
        let entry = self.sign_entry(
            EntryBody::Proposal {
                kind: "replicate".into(),
                payload: recommendation.payload_hash(),
                deadline_ms: now_ms + REPLICATION_VOTING_MS,
            },
            now_ms,
        )?;
        let id = entry.id();
        self.ledger.submit(entry.clone(), now_ms)?;
        self.replication.push((id, recommendation));
        if self.replication.len() > 32 {
            self.replication.remove(0);
        }
        Ok(if self.ledger_gossip() {
            vec![Outbound::broadcast(Wire::LedgerEntry { entry })]
        } else {
            Vec::new()
        })
    }

    /// Periodic work: lifecycle, registration, block production, round
    /// changes, anti-entropy and governed replication.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Outbound> {
        let window = std::mem::take(&mut self.window);
        let completed = window.successes;
        let signals = Signals {
            successes: window.successes,
            failures: window.failures,
            mean_latency_ms: (completed > 0).then(|| window.latency_sum_ms / f64::from(completed)),
            target_latency_ms: self.config.target_latency_ms,
            in_flight: self.in_flight,
            capacity: self.config.capacity,
            reputation_milli: Some(self.ledger.state().reputation(&self.node_id())),
        };
        self.lifecycle.tick(&signals, now_ms);

        let mut out = Vec::new();
        let steps: [TickStep; 3] = [
            Self::ensure_registered,
            Self::try_propose,
            Self::consider_replication,
        ];
        for step in steps {
            match step(self, now_ms) {
                Ok(mut messages) => out.append(&mut messages),
                Err(error) => tracing::debug!(%error, "darm tick step failed"),
            }
        }
        if self.ledger.pending_len() > 0
            && now_ms.saturating_sub(self.last_progress_ms)
                > ROUND_TIMEOUT_MS.saturating_mul(u64::from(self.round) + 1)
        {
            self.round = self.round.saturating_add(1);
            self.last_progress_ms = now_ms;
        }
        if self.memory_gossip()
            && now_ms.saturating_sub(self.last_announce_ms) >= ANNOUNCE_INTERVAL_MS
        {
            self.last_announce_ms = now_ms;
            let ids = self.memory.inventory(crate::wire::MAX_IDS);
            if !ids.is_empty() {
                out.push(Outbound::broadcast(Wire::MemoryAnnounce { ids }));
            }
        }
        out
    }

    pub fn on_peer_up(&mut self, peer: &str) -> Vec<Outbound> {
        let mut out = Vec::new();
        if self.ledger_gossip() {
            out.push(Outbound::to(
                peer,
                Wire::SyncRequest {
                    chain: self.ledger.chain_id(),
                    from_height: self.ledger.height(),
                },
            ));
        }
        if self.memory_gossip() {
            let ids = self.memory.inventory(crate::wire::MAX_IDS);
            if !ids.is_empty() {
                out.push(Outbound::to(peer, Wire::MemoryAnnounce { ids }));
            }
        }
        out
    }

    fn admit(&mut self, source: &str, now_ms: u64) -> bool {
        if !self.buckets.contains_key(source) && self.buckets.len() >= MAX_TRACKED_PEERS {
            self.buckets
                .retain(|_, b| now_ms.saturating_sub(b.updated_ms) < 60_000);
            if self.buckets.len() >= MAX_TRACKED_PEERS {
                return false;
            }
        }
        let bucket = self.buckets.entry(source.to_string()).or_insert(Bucket {
            tokens: BUCKET_CAPACITY,
            updated_ms: now_ms,
        });
        let elapsed = now_ms.saturating_sub(bucket.updated_ms) as f64 / 1_000.0;
        bucket.tokens = (bucket.tokens + elapsed * BUCKET_REFILL_PER_SEC).min(BUCKET_CAPACITY);
        bucket.updated_ms = now_ms;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }

    /// Handle one inbound gossip message from `source` (an unauthenticated
    /// peer id used only for replies and rate limiting).
    pub fn handle(&mut self, source: &str, message: Wire, now_ms: u64) -> Vec<Outbound> {
        if !self.admit(source, now_ms) {
            self.counters.gossip_rate_limited += 1;
            return Vec::new();
        }
        self.counters.gossip_received += 1;
        match self.handle_inner(source, message, now_ms) {
            Ok(out) => out,
            Err(error) => {
                self.counters.gossip_rejected += 1;
                tracing::debug!(%error, source, "darm gossip rejected");
                Vec::new()
            }
        }
    }

    fn handle_inner(&mut self, source: &str, message: Wire, now_ms: u64) -> Result<Vec<Outbound>> {
        let memory_gossip = self.memory_gossip();
        let ledger_gossip = self.ledger_gossip();
        let mut out = Vec::new();
        match message {
            Wire::MemoryAnnounce { ids } if memory_gossip => {
                let mut missing = self.memory.missing(&ids);
                missing.truncate(MAX_REQUEST_IDS);
                if !missing.is_empty() {
                    out.push(Outbound::to(source, Wire::MemoryRequest { ids: missing }));
                }
            }
            Wire::MemoryRequest { ids } if memory_gossip => {
                let wanted: Vec<Hash32> = ids.into_iter().take(MAX_REQUEST_IDS).collect();
                for chunk in self
                    .memory
                    .export_shareable(&wanted)
                    .chunks(MEMORIES_PER_PUSH)
                {
                    out.push(Outbound::to(
                        source,
                        Wire::MemoryPush {
                            memories: chunk.to_vec(),
                        },
                    ));
                }
            }
            Wire::MemoryPush { memories } if memory_gossip => {
                for memory in memories {
                    if self.memory.insert(memory, Origin::Peer, now_ms).is_ok() {
                        self.counters.memories_from_peers += 1;
                    }
                }
            }
            Wire::Tombstone { tombstone } => {
                self.memory.apply_tombstone(tombstone)?;
            }
            Wire::LedgerEntry { entry } if ledger_gossip => {
                self.ledger.submit(entry, now_ms)?;
            }
            Wire::BlockProposal { block } if ledger_gossip => {
                if block.header.height > self.ledger.height() + 1 {
                    out.push(Outbound::to(
                        source,
                        Wire::SyncRequest {
                            chain: self.ledger.chain_id(),
                            from_height: self.ledger.height(),
                        },
                    ));
                    return Ok(out);
                }
                let (hash, _) = self.ledger.receive_proposal(block, now_ms)?;
                if self.ledger.is_validator(&self.node_id())
                    && self.ledger.candidate(&hash).is_some()
                    && let Ok((attestation, _)) = self.ledger.attest(&self.identity, &hash)
                {
                    out.push(Outbound::broadcast(Wire::Attestation {
                        block_hash: hash,
                        attestation,
                    }));
                }
                self.sync_effects()?;
            }
            Wire::Attestation {
                block_hash,
                attestation,
            } if ledger_gossip => {
                if self.ledger.candidate(&block_hash).is_some() {
                    self.ledger.receive_attestation(&block_hash, attestation)?;
                    self.sync_effects()?;
                }
            }
            Wire::SyncRequest { chain, from_height } if ledger_gossip => {
                if chain != self.ledger.chain_id() {
                    return Err(DarmError::Rejected("sync for another chain".into()));
                }
                let blocks: Vec<_> = self
                    .ledger
                    .blocks()
                    .iter()
                    .skip(usize::try_from(from_height).unwrap_or(usize::MAX))
                    .take(MAX_BLOCKS_PER_SYNC)
                    .cloned()
                    .collect();
                if !blocks.is_empty() {
                    out.push(Outbound::to(source, Wire::SyncBlocks { blocks }));
                }
            }
            Wire::SyncBlocks { blocks } if ledger_gossip => {
                let full = blocks.len() == MAX_BLOCKS_PER_SYNC;
                for block in blocks {
                    if block.header.height == self.ledger.height() + 1 {
                        self.ledger.commit_finalized(block, Some(now_ms))?;
                    }
                }
                self.sync_effects()?;
                if full {
                    out.push(Outbound::to(
                        source,
                        Wire::SyncRequest {
                            chain: self.ledger.chain_id(),
                            from_height: self.ledger.height(),
                        },
                    ));
                }
            }
            // Gossip of a kind this node has disabled.
            _ => {}
        }
        Ok(out)
    }

    /// Finalized blocks above `height`, for mirroring to an archive.
    pub fn blocks_after(&self, height: u64, limit: usize) -> Vec<darm_core::ledger::Block> {
        self.ledger
            .blocks()
            .iter()
            .skip(usize::try_from(height).unwrap_or(usize::MAX))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Mesh-scoped memories not yet accepted by the archive.
    pub fn unarchived_memories(&self, limit: usize) -> Vec<SignedMemory> {
        let ids: Vec<Hash32> = self
            .memory
            .inventory(usize::MAX)
            .into_iter()
            .filter(|id| !self.archived.contains(id))
            .take(limit)
            .collect();
        self.memory.export_shareable(&ids)
    }

    pub fn mark_archived(&mut self, ids: impl IntoIterator<Item = Hash32>) {
        self.archived.extend(ids);
    }

    pub fn status(&self, now_ms: u64) -> Value {
        let ledger = &self.ledger;
        let validators = ledger.config().validators.clone();
        let proposals: Vec<Value> = ledger
            .state()
            .proposals()
            .map(|(id, p)| {
                let outcome: ProposalOutcome = p.outcome(&validators, now_ms);
                json!({
                    "id": id,
                    "kind": p.kind,
                    "payload": p.payload,
                    "author": p.author,
                    "deadline_ms": p.deadline_ms,
                    "yes": p.yes.len(),
                    "no": p.no.len(),
                    "outcome": outcome,
                })
            })
            .collect();
        let routes: Vec<Value> = self
            .pheromones
            .snapshot(now_ms)
            .into_iter()
            .take(16)
            .map(|(route, kind, intensity)| {
                json!({"node": route.node, "model": route.model, "kind": kind, "intensity": intensity})
            })
            .collect();
        json!({
            "node_id": self.node_id(),
            "role": self.lifecycle.role,
            "state": self.lifecycle.state,
            "vitals": self.lifecycle.vitals,
            "genome": {"commitment": self.genome.commitment(), "generation": self.genome.generation, "specializations": self.genome.specializations},
            "public_mesh": self.public_mesh,
            "memory_gossip": self.memory_gossip(),
            "ledger_gossip": self.ledger_gossip(),
            "ledger": {
                "chain": ledger.config().name,
                "chain_id": ledger.chain_id(),
                "height": ledger.height(),
                "head": ledger.head_hash(),
                "pending": ledger.pending_len(),
                "round": self.round,
                "validators": validators.members().map(|(n, w)| json!({"node": n, "weight": w})).collect::<Vec<_>>(),
                "is_validator": ledger.is_validator(&self.node_id()),
                "committed_memories": ledger.state().committed_memories(),
                "registered_nodes": ledger.state().registrations().count(),
                "proposals": proposals,
            },
            "memory": self.memory.stats(),
            "pheromones": routes,
            "replication_recommendations": self.replication.iter().map(|(id, r)| json!({"proposal": id, "recommendation": r})).collect::<Vec<_>>(),
            "counters": self.counters,
        })
    }
}

fn chain_config(config: &DarmConfig, identity: &NodeIdentity) -> Result<ChainConfig> {
    let validators = if config.validators.is_empty() {
        ValidatorSet::equal([identity.node_id()])?
    } else {
        ValidatorSet::equal(config.validators.iter().copied())?
    };
    Ok(ChainConfig {
        name: config
            .chain_name
            .clone()
            .unwrap_or_else(|| format!("darm-local-{}", identity.node_id().short())),
        validators,
    })
}

pub fn namespace_tag(namespace: &str) -> String {
    let cleaned = sanitize_tag(namespace);
    format!(
        "ns:{}",
        if cleaned.is_empty() {
            "default"
        } else {
            &cleaned
        }
    )
}

/// Map arbitrary text onto the memory tag alphabet `[A-Za-z0-9_.:-]`.
fn sanitize_tag(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect()
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut out: String = value.chars().take(max).collect();
    if value.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(unix)]
fn restrict_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests;
