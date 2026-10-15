use crate::embed::{Embedder, HashingEmbedder, cosine};
use crate::record::{MemoryRecord, Scope, SignedMemory, SignedTombstone};
use darm_core::{DarmError, Hash32, NodeId, NodeIdentity, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;

const DAY_MS: f64 = 86_400_000.0;
/// Short-term memories halve in strength every day, long-term every month.
const SHORT_TERM_HALF_LIFE_MS: f64 = DAY_MS;
const LONG_TERM_HALF_LIFE_MS: f64 = 30.0 * DAY_MS;
/// Recalls needed before a memory is consolidated into long-term storage.
const CONSOLIDATION_RECALLS: u32 = 3;
const CONSOLIDATION_IMPORTANCE_MILLI: u16 = 800;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    ShortTerm,
    LongTerm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Written by this node.
    Local,
    /// Received from a peer.
    Peer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    Duplicate,
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryLimits {
    pub max_records: usize,
    /// Cap on records any single remote author may occupy, so one peer
    /// cannot flood the store and evict everyone else.
    pub max_records_per_peer: usize,
    pub max_tombstones: usize,
    /// Accept peer records created at most this far in the future.
    pub max_clock_skew_ms: u64,
}

impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            max_records: 50_000,
            max_records_per_peer: 5_000,
            max_tombstones: 100_000,
            max_clock_skew_ms: 5 * 60 * 1_000,
        }
    }
}

#[derive(Debug, Clone)]
struct Stored {
    signed: SignedMemory,
    embedding: Vec<f32>,
    origin: Origin,
    tier: Tier,
    recalls: u32,
    last_recalled_ms: u64,
    finalized: bool,
}

impl Stored {
    fn strength(&self, now_ms: u64) -> f32 {
        let record = &self.signed.record;
        let reference = record.created_ms.max(self.last_recalled_ms);
        let age = now_ms.saturating_sub(reference) as f64;
        let half_life = match self.tier {
            Tier::ShortTerm => SHORT_TERM_HALF_LIFE_MS,
            Tier::LongTerm => LONG_TERM_HALF_LIFE_MS,
        };
        let recency = 0.5f64.powf(age / half_life) as f32;
        let rehearsal = 1.0 + (1.0 + self.recalls as f32).ln() * 0.2;
        (0.3 + 0.7 * record.importance()) * (0.5 + 0.5 * recency) * rehearsal
    }

    fn maybe_consolidate(&mut self) {
        if self.tier == Tier::ShortTerm
            && (self.finalized
                || self.recalls >= CONSOLIDATION_RECALLS
                || self.signed.record.importance_milli >= CONSOLIDATION_IMPORTANCE_MILLI)
        {
            self.tier = Tier::LongTerm;
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: Hash32,
    pub score: f32,
    pub similarity: f32,
    pub tier: Tier,
    pub finalized: bool,
    pub record: MemoryRecord,
}

pub struct SearchOptions<'a> {
    pub limit: usize,
    pub min_similarity: f32,
    /// Only return memories with these scopes.
    pub scopes: &'a [Scope],
    /// Trust weight in `[0, 1]` per author; `0` excludes the author.
    pub trust: &'a dyn Fn(&NodeId) -> f32,
    /// Only return ledger-finalized memories (or the node's own).
    pub require_finalized_from_peers: bool,
    /// Only return memories carrying this tag (namespace isolation).
    pub required_tag: Option<&'a str>,
}

impl Default for SearchOptions<'_> {
    fn default() -> Self {
        Self {
            limit: 5,
            min_similarity: 0.15,
            scopes: &[Scope::Local, Scope::Mesh],
            trust: &|_| 1.0,
            require_finalized_from_peers: false,
            required_tag: None,
        }
    }
}

/// Persisted event, replayed (and re-verified) on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum LogEvent {
    Insert { memory: SignedMemory, local: bool },
    Tombstone { tombstone: SignedTombstone },
    Finalized { id: Hash32 },
}

/// Append-only JSONL persistence for the memory store.
#[derive(Debug, Clone)]
pub struct MemoryLog {
    path: PathBuf,
}

impl MemoryLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn append(&self, event: &LogEvent) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_vec(event)
            .map_err(|e| DarmError::malformed("memory log", e.to_string()))?;
        line.push(b'\n');
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }

    fn events(&self) -> Result<Vec<LogEvent>> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut events = Vec::new();
        for (number, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            events.push(serde_json::from_str(&line).map_err(|e| {
                DarmError::malformed("memory log", format!("line {}: {e}", number + 1))
            })?);
        }
        Ok(events)
    }
}

pub struct MemoryStore {
    records: HashMap<Hash32, Stored>,
    tombstones: HashMap<Hash32, SignedTombstone>,
    per_author: HashMap<NodeId, usize>,
    embedder: Arc<dyn Embedder>,
    limits: MemoryLimits,
    log: Option<MemoryLog>,
}

impl MemoryStore {
    pub fn new(limits: MemoryLimits) -> Self {
        Self {
            records: HashMap::new(),
            tombstones: HashMap::new(),
            per_author: HashMap::new(),
            embedder: Arc::new(HashingEmbedder::default()),
            limits,
            log: None,
        }
    }

    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = embedder;
        let ids: Vec<Hash32> = self.records.keys().copied().collect();
        for id in ids {
            if let Some(stored) = self.records.get_mut(&id) {
                stored.embedding = self.embedder.embed(&stored.signed.record.content);
            }
        }
        self
    }

    /// Open a persistent store. Every logged record and tombstone is
    /// re-verified; a log that fails verification is an error, not a
    /// silently partial store.
    pub fn open(log: MemoryLog, limits: MemoryLimits) -> Result<Self> {
        let mut store = Self::new(limits);
        for event in log.events()? {
            match event {
                LogEvent::Insert { memory, local } => {
                    let origin = if local { Origin::Local } else { Origin::Peer };
                    // Clock checks are for live gossip, not for history.
                    store.insert_inner(memory, origin, u64::MAX)?;
                }
                LogEvent::Tombstone { tombstone } => {
                    store.apply_tombstone_inner(tombstone)?;
                }
                LogEvent::Finalized { id } => {
                    store.mark_finalized_inner(&id);
                }
            }
        }
        store.log = Some(log);
        Ok(store)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn get(&self, id: &Hash32) -> Option<&SignedMemory> {
        self.records.get(id).map(|s| &s.signed)
    }

    pub fn contains(&self, id: &Hash32) -> bool {
        self.records.contains_key(id) || self.tombstones.contains_key(id)
    }

    pub fn tier(&self, id: &Hash32) -> Option<Tier> {
        self.records.get(id).map(|s| s.tier)
    }

    /// Sign and store a memory written by this node.
    pub fn remember(
        &mut self,
        identity: &NodeIdentity,
        record: MemoryRecord,
    ) -> Result<SignedMemory> {
        let signed = SignedMemory::sign(identity, record)?;
        self.insert(signed.clone(), Origin::Local, signed.record.created_ms)?;
        Ok(signed)
    }

    /// Insert a signed memory (own or from a peer) after verifying it.
    pub fn insert(
        &mut self,
        signed: SignedMemory,
        origin: Origin,
        now_ms: u64,
    ) -> Result<InsertOutcome> {
        let outcome = self.insert_inner(signed.clone(), origin, now_ms)?;
        if outcome == InsertOutcome::Inserted
            && let Some(log) = &self.log
        {
            log.append(&LogEvent::Insert {
                memory: signed,
                local: origin == Origin::Local,
            })?;
        }
        Ok(outcome)
    }

    fn insert_inner(
        &mut self,
        signed: SignedMemory,
        origin: Origin,
        now_ms: u64,
    ) -> Result<InsertOutcome> {
        let id = signed.id();
        if self.tombstones.contains_key(&id) {
            return Err(DarmError::Rejected(
                "memory was forgotten by its author".into(),
            ));
        }
        if self.records.contains_key(&id) {
            return Ok(InsertOutcome::Duplicate);
        }
        signed.verify()?;
        let record = &signed.record;
        if origin == Origin::Peer {
            if record.scope != Scope::Mesh {
                return Err(DarmError::Rejected(
                    "peers may only share mesh-scoped memories".into(),
                ));
            }
            if record.created_ms > now_ms.saturating_add(self.limits.max_clock_skew_ms) {
                return Err(DarmError::Rejected("memory created in the future".into()));
            }
            let count = self.per_author.get(&record.author).copied().unwrap_or(0);
            if count >= self.limits.max_records_per_peer {
                return Err(DarmError::LimitExceeded(format!(
                    "author {} reached its memory quota",
                    record.author.short()
                )));
            }
        }
        if self.records.len() >= self.limits.max_records {
            self.evict_one(now_ms)?;
        }
        let embedding = self.embedder.embed(&record.content);
        *self.per_author.entry(record.author).or_default() += 1;
        let mut stored = Stored {
            signed,
            embedding,
            origin,
            tier: Tier::ShortTerm,
            recalls: 0,
            last_recalled_ms: 0,
            finalized: false,
        };
        stored.maybe_consolidate();
        self.records.insert(id, stored);
        Ok(InsertOutcome::Inserted)
    }

    /// Evict the weakest short-term memory received from a peer. Local and
    /// long-term memories are never evicted to make room for gossip.
    fn evict_one(&mut self, now_ms: u64) -> Result<()> {
        let victim = self
            .records
            .iter()
            .filter(|(_, s)| s.tier == Tier::ShortTerm && s.origin == Origin::Peer)
            .min_by(|a, b| a.1.strength(now_ms).total_cmp(&b.1.strength(now_ms)))
            .map(|(id, _)| *id);
        match victim {
            Some(id) => {
                self.remove(&id);
                Ok(())
            }
            None => Err(DarmError::LimitExceeded("memory store is full".into())),
        }
    }

    fn remove(&mut self, id: &Hash32) -> Option<Stored> {
        let stored = self.records.remove(id)?;
        let author = stored.signed.record.author;
        if let Some(count) = self.per_author.get_mut(&author) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_author.remove(&author);
            }
        }
        Some(stored)
    }

    /// Forget one of this node's own memories and return the tombstone to
    /// gossip. Nodes cannot forget other authors' memories for the mesh.
    pub fn forget(
        &mut self,
        identity: &NodeIdentity,
        id: &Hash32,
        now_ms: u64,
    ) -> Result<SignedTombstone> {
        let Some(stored) = self.records.get(id) else {
            return Err(DarmError::Rejected("unknown memory".into()));
        };
        if stored.signed.record.author != identity.node_id() {
            return Err(DarmError::Unauthorized(
                "only the author can forget a memory".into(),
            ));
        }
        let tombstone = SignedTombstone::sign(identity, *id, now_ms);
        self.apply_tombstone(tombstone.clone())?;
        Ok(tombstone)
    }

    pub fn apply_tombstone(&mut self, tombstone: SignedTombstone) -> Result<bool> {
        let applied = self.apply_tombstone_inner(tombstone.clone())?;
        if applied && let Some(log) = &self.log {
            log.append(&LogEvent::Tombstone { tombstone })?;
        }
        Ok(applied)
    }

    fn apply_tombstone_inner(&mut self, tombstone: SignedTombstone) -> Result<bool> {
        if self.tombstones.contains_key(&tombstone.memory) {
            return Ok(false);
        }
        tombstone.verify()?;
        match self.records.get(&tombstone.memory) {
            Some(stored) if stored.signed.record.author != tombstone.author => {
                return Err(DarmError::Unauthorized("tombstone author mismatch".into()));
            }
            Some(_) => {
                self.remove(&tombstone.memory);
            }
            // Unknown record: only keep tombstones while there is room, so
            // they cannot be used to fill memory.
            None if self.tombstones.len() >= self.limits.max_tombstones => return Ok(false),
            None => {}
        }
        self.tombstones.insert(tombstone.memory, tombstone);
        Ok(true)
    }

    /// The ledger finalized this memory's commit: consolidate it.
    pub fn mark_finalized(&mut self, id: &Hash32) -> Result<bool> {
        let changed = self.mark_finalized_inner(id);
        if changed && let Some(log) = &self.log {
            log.append(&LogEvent::Finalized { id: *id })?;
        }
        Ok(changed)
    }

    fn mark_finalized_inner(&mut self, id: &Hash32) -> bool {
        match self.records.get_mut(id) {
            Some(stored) if !stored.finalized => {
                stored.finalized = true;
                stored.maybe_consolidate();
                true
            }
            _ => false,
        }
    }

    /// Associative recall: cosine similarity weighted by memory strength and
    /// author trust. Recalled memories are rehearsed (strengthened).
    pub fn search(&mut self, query: &str, options: &SearchOptions<'_>, now_ms: u64) -> Vec<Hit> {
        let query_vector = self.embedder.embed(query);
        if query_vector.iter().all(|v| *v == 0.0) {
            return Vec::new();
        }
        let mut hits: Vec<Hit> = self
            .records
            .iter()
            .filter_map(|(id, stored)| {
                let record = &stored.signed.record;
                if !options.scopes.contains(&record.scope) {
                    return None;
                }
                if let Some(tag) = options.required_tag
                    && !record.tags.iter().any(|t| t == tag)
                {
                    return None;
                }
                if options.require_finalized_from_peers
                    && stored.origin == Origin::Peer
                    && !stored.finalized
                {
                    return None;
                }
                let trust = (options.trust)(&record.author).clamp(0.0, 1.0);
                if trust <= 0.0 {
                    return None;
                }
                let similarity = cosine(&query_vector, &stored.embedding);
                if similarity < options.min_similarity {
                    return None;
                }
                let strength = stored.strength(now_ms).min(1.5) / 1.5;
                Some(Hit {
                    id: *id,
                    score: similarity * (0.6 + 0.4 * strength) * trust,
                    similarity,
                    tier: stored.tier,
                    finalized: stored.finalized,
                    record: record.clone(),
                })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(options.limit);
        for hit in &hits {
            if let Some(stored) = self.records.get_mut(&hit.id) {
                stored.recalls = stored.recalls.saturating_add(1);
                stored.last_recalled_ms = now_ms;
                stored.maybe_consolidate();
            }
        }
        hits
    }

    /// Ids of shareable (mesh-scoped) memories, newest first, for gossip
    /// anti-entropy.
    pub fn inventory(&self, limit: usize) -> Vec<Hash32> {
        let mut shareable: Vec<(&Hash32, u64)> = self
            .records
            .iter()
            .filter(|(_, s)| s.signed.record.scope == Scope::Mesh)
            .map(|(id, s)| (id, s.signed.record.created_ms))
            .collect();
        shareable.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        shareable
            .into_iter()
            .take(limit)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Which of a peer's advertised ids this node does not have yet.
    pub fn missing(&self, advertised: &[Hash32]) -> Vec<Hash32> {
        let mut seen = HashSet::new();
        advertised
            .iter()
            .filter(|id| seen.insert(**id) && !self.contains(id))
            .copied()
            .collect()
    }

    /// Records a peer may receive: mesh-scoped only, never local ones.
    pub fn export_shareable(&self, ids: &[Hash32]) -> Vec<SignedMemory> {
        ids.iter()
            .filter_map(|id| self.records.get(id))
            .filter(|s| s.signed.record.scope == Scope::Mesh)
            .map(|s| s.signed.clone())
            .collect()
    }

    pub fn stats(&self) -> MemoryStats {
        let mut stats = MemoryStats::default();
        for stored in self.records.values() {
            match stored.tier {
                Tier::ShortTerm => stats.short_term += 1,
                Tier::LongTerm => stats.long_term += 1,
            }
            if stored.finalized {
                stats.finalized += 1;
            }
            match stored.origin {
                Origin::Local => stats.local += 1,
                Origin::Peer => stats.from_peers += 1,
            }
        }
        stats.tombstones = self.tombstones.len();
        stats
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MemoryStats {
    pub short_term: usize,
    pub long_term: usize,
    pub finalized: usize,
    pub local: usize,
    pub from_peers: usize,
    pub tombstones: usize,
}

#[cfg(test)]
mod tests;
