use super::entry::{
    Attestation, Block, BlockHeader, SignedEntry, attestation_message, block_proposal_message,
};
use super::state::LedgerState;
use super::validators::ChainConfig;
use crate::{DarmError, Hash32, NodeId, NodeIdentity, Result, merkle};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Resource bounds. Every limit exists so a peer cannot make another node
/// allocate without bound by gossiping entries, proposals or attestations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerLimits {
    pub max_entries_per_block: usize,
    pub max_pending: usize,
    pub max_pending_per_author: usize,
    pub max_candidates: usize,
    /// How far in the future a timestamp may be relative to the local clock.
    pub max_clock_skew_ms: u64,
}

impl Default for LedgerLimits {
    fn default() -> Self {
        Self {
            max_entries_per_block: 512,
            max_pending: 10_000,
            max_pending_per_author: 1_000,
            max_candidates: 64,
            max_clock_skew_ms: 5 * 60 * 1_000,
        }
    }
}

#[derive(Debug)]
pub struct Ledger {
    config: ChainConfig,
    chain_id: Hash32,
    genesis: Hash32,
    limits: LedgerLimits,
    blocks: Vec<Block>,
    state: LedgerState,
    committed_entries: HashMap<Hash32, (u64, usize)>,
    pending: BTreeMap<(u64, Hash32), SignedEntry>,
    /// Entry id -> timestamp, the other half of the `pending` key.
    pending_ids: HashMap<Hash32, u64>,
    pending_per_author: HashMap<NodeId, usize>,
    candidates: HashMap<Hash32, Block>,
    /// Height -> block hash this node attested to. One vote per height is the
    /// safety rule; never relax it.
    local_votes: HashMap<u64, Hash32>,
}

impl Ledger {
    pub fn new(config: ChainConfig) -> Self {
        Self::with_limits(config, LedgerLimits::default())
    }

    pub fn with_limits(config: ChainConfig, limits: LedgerLimits) -> Self {
        Self {
            chain_id: config.chain_id(),
            genesis: config.genesis_hash(),
            config,
            limits,
            blocks: Vec::new(),
            state: LedgerState::default(),
            committed_entries: HashMap::new(),
            pending: BTreeMap::new(),
            pending_ids: HashMap::new(),
            pending_per_author: HashMap::new(),
            candidates: HashMap::new(),
            local_votes: HashMap::new(),
        }
    }

    /// Rebuild a ledger from finalized blocks, verifying every block,
    /// signature, attestation quorum and state transition on the way.
    pub fn replay(config: ChainConfig, blocks: Vec<Block>) -> Result<Self> {
        let mut ledger = Self::new(config);
        for block in blocks {
            ledger.commit_finalized(block, None)?;
        }
        Ok(ledger)
    }

    pub fn config(&self) -> &ChainConfig {
        &self.config
    }

    pub fn chain_id(&self) -> Hash32 {
        self.chain_id
    }

    pub fn height(&self) -> u64 {
        self.blocks.len() as u64
    }

    pub fn head_hash(&self) -> Hash32 {
        self.blocks.last().map_or(self.genesis, Block::hash)
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn block(&self, height: u64) -> Option<&Block> {
        height
            .checked_sub(1)
            .and_then(|index| self.blocks.get(index as usize))
    }

    pub fn state(&self) -> &LedgerState {
        &self.state
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_validator(&self, node: &NodeId) -> bool {
        self.config.validators.contains(node)
    }

    pub fn proposer_for_next(&self, round: u32) -> NodeId {
        self.config
            .validators
            .proposer_for(self.height() + 1, round)
    }

    pub fn contains_entry(&self, id: &Hash32) -> bool {
        self.committed_entries.contains_key(id) || self.pending_ids.contains_key(id)
    }

    /// Where a finalized entry lives, plus a Merkle proof against that
    /// block's `entries_root`.
    pub fn locate_entry(&self, id: &Hash32) -> Option<(u64, merkle::Proof)> {
        let (height, _) = self.committed_entries.get(id)?;
        let proof = self.block(*height)?.prove_entry(id)?;
        Some((*height, proof))
    }

    /// Admit a signed entry to the pending pool. Returns `Ok(false)` when it
    /// is already known, so gossip handlers can stop re-broadcasting it.
    pub fn submit(&mut self, entry: SignedEntry, now_ms: u64) -> Result<bool> {
        let id = entry.id();
        if self.contains_entry(&id) {
            return Ok(false);
        }
        entry.verify(&self.chain_id)?;
        if entry.entry.timestamp_ms > now_ms.saturating_add(self.limits.max_clock_skew_ms) {
            return Err(DarmError::Rejected(
                "entry timestamp is in the future".into(),
            ));
        }
        if self.pending.len() >= self.limits.max_pending {
            return Err(DarmError::LimitExceeded("pending pool full".into()));
        }
        let author = entry.entry.author;
        let per_author = self.pending_per_author.entry(author).or_default();
        if *per_author >= self.limits.max_pending_per_author {
            return Err(DarmError::LimitExceeded(format!(
                "author {} has too many pending entries",
                author.short()
            )));
        }
        *per_author += 1;
        self.pending_ids.insert(id, entry.entry.timestamp_ms);
        self.pending.insert((entry.entry.timestamp_ms, id), entry);
        Ok(true)
    }

    /// Build and sign the next block if `identity` is the designated proposer
    /// for `round`. Entries that would fail state rules are dropped from the
    /// pool. Returns `None` when there is nothing to propose.
    pub fn propose(
        &mut self,
        identity: &NodeIdentity,
        round: u32,
        now_ms: u64,
    ) -> Result<Option<Block>> {
        let proposer = identity.node_id();
        if self.proposer_for_next(round) != proposer {
            return Err(DarmError::Unauthorized(
                "not the designated proposer".into(),
            ));
        }
        let timestamp_ms = now_ms.max(self.blocks.last().map_or(0, |b| b.header.timestamp_ms));
        let mut scratch = self.state.clone();
        let mut chosen = Vec::new();
        let mut rejected = Vec::new();
        for (key, entry) in &self.pending {
            if chosen.len() >= self.limits.max_entries_per_block {
                break;
            }
            match scratch.apply(entry, timestamp_ms, &self.config.validators) {
                Ok(()) => chosen.push(entry.clone()),
                Err(_) => rejected.push(*key),
            }
        }
        for key in rejected {
            self.remove_pending(&key);
        }
        if chosen.is_empty() {
            return Ok(None);
        }
        let ids: Vec<Hash32> = chosen.iter().map(SignedEntry::id).collect();
        let header = BlockHeader {
            chain: self.chain_id,
            height: self.height() + 1,
            round,
            prev: self.head_hash(),
            entries_root: merkle::root(&ids),
            entry_count: ids.len() as u32,
            timestamp_ms,
            proposer,
            validators: self.config.validators.commitment(),
        };
        let hash = header.hash();
        let block = Block {
            proposer_signature: identity.sign(&block_proposal_message(&self.chain_id, &hash)),
            header,
            entries: chosen,
            attestations: Vec::new(),
        };
        self.insert_candidate(block.clone())?;
        Ok(Some(block))
    }

    /// Validate a proposal received from a peer and keep it as a candidate.
    /// Any attestations it carries are verified and kept. Returns the block
    /// hash and whether the candidate reached finality as a result.
    pub fn receive_proposal(&mut self, block: Block, now_ms: u64) -> Result<(Hash32, bool)> {
        if block.header.timestamp_ms > now_ms.saturating_add(self.limits.max_clock_skew_ms) {
            return Err(DarmError::Rejected(
                "block timestamp is in the future".into(),
            ));
        }
        let hash = block.hash();
        if self.blocks.iter().any(|b| b.hash() == hash) {
            return Ok((hash, false));
        }
        let attestations = block.attestations.clone();
        let mut bare = block;
        bare.attestations.clear();
        if !self.candidates.contains_key(&hash) {
            self.insert_candidate(bare)?;
        }
        let mut finalized = false;
        for attestation in attestations {
            if let Some(_height) = self.receive_attestation(&hash, attestation)? {
                finalized = true;
                break;
            }
        }
        Ok((hash, finalized))
    }

    /// Attest to a candidate as `identity`. Refuses to sign a second block at
    /// a height this node already voted on.
    pub fn attest(
        &mut self,
        identity: &NodeIdentity,
        block_hash: &Hash32,
    ) -> Result<(Attestation, Option<u64>)> {
        let validator = identity.node_id();
        if !self.is_validator(&validator) {
            return Err(DarmError::Unauthorized("not a validator".into()));
        }
        let Some(candidate) = self.candidates.get(block_hash) else {
            return Err(DarmError::Rejected("unknown candidate".into()));
        };
        let height = candidate.header.height;
        match self.local_votes.get(&height) {
            Some(voted) if voted != block_hash => {
                return Err(DarmError::Rejected(
                    "already attested a different block at this height".into(),
                ));
            }
            _ => {}
        }
        self.local_votes.insert(height, *block_hash);
        let attestation = Attestation {
            validator,
            signature: identity.sign(&attestation_message(&self.chain_id, height, block_hash)),
        };
        let finalized = self.receive_attestation(block_hash, attestation.clone())?;
        Ok((attestation, finalized))
    }

    /// Record a validator's attestation; commits the candidate once quorum is
    /// reached and returns the committed height.
    pub fn receive_attestation(
        &mut self,
        block_hash: &Hash32,
        attestation: Attestation,
    ) -> Result<Option<u64>> {
        if !self.is_validator(&attestation.validator) {
            return Err(DarmError::Unauthorized(
                "attestation from non-validator".into(),
            ));
        }
        let chain_id = self.chain_id;
        let Some(candidate) = self.candidates.get_mut(block_hash) else {
            return Err(DarmError::Rejected("unknown candidate".into()));
        };
        let message = attestation_message(&chain_id, candidate.header.height, block_hash);
        attestation
            .validator
            .verify(&message, &attestation.signature)?;
        if candidate
            .attestations
            .iter()
            .any(|a| a.validator == attestation.validator)
        {
            return Ok(None);
        }
        candidate.attestations.push(attestation);
        let quorum = self
            .config
            .validators
            .has_quorum(candidate.attestations.iter().map(|a| &a.validator));
        if !quorum {
            return Ok(None);
        }
        let block = self
            .candidates
            .remove(block_hash)
            .expect("candidate was just borrowed");
        let height = block.header.height;
        self.commit_finalized(block, None)?;
        Ok(Some(height))
    }

    pub fn candidate(&self, block_hash: &Hash32) -> Option<&Block> {
        self.candidates.get(block_hash)
    }

    /// Append a block that already carries a quorum of attestations (sync,
    /// replay, or the last attestation arriving). Verifies everything.
    pub fn commit_finalized(&mut self, block: Block, now_ms: Option<u64>) -> Result<()> {
        if let Some(now_ms) = now_ms
            && block.header.timestamp_ms > now_ms.saturating_add(self.limits.max_clock_skew_ms)
        {
            return Err(DarmError::Rejected(
                "block timestamp is in the future".into(),
            ));
        }
        let next_state = self.validate_body(&block)?;
        self.verify_quorum(&block)?;
        let height = block.header.height;
        for (index, entry) in block.entries.iter().enumerate() {
            let id = entry.id();
            self.committed_entries.insert(id, (height, index));
            if let Some(timestamp) = self.pending_ids.get(&id).copied() {
                self.remove_pending(&(timestamp, id));
            }
        }
        self.state = next_state;
        self.blocks.push(block);
        self.candidates.retain(|_, c| c.header.height > height);
        self.local_votes.retain(|h, _| *h > height);
        Ok(())
    }

    fn insert_candidate(&mut self, block: Block) -> Result<()> {
        self.validate_body(&block)?;
        if self.candidates.len() >= self.limits.max_candidates {
            return Err(DarmError::LimitExceeded("too many candidate blocks".into()));
        }
        self.candidates.insert(block.hash(), block);
        Ok(())
    }

    /// Everything except attestation quorum. Returns the state after
    /// applying the block so callers do not replay it twice.
    fn validate_body(&self, block: &Block) -> Result<LedgerState> {
        let header = &block.header;
        if header.chain != self.chain_id {
            return Err(DarmError::Rejected("block is for another chain".into()));
        }
        if header.height != self.height() + 1 {
            return Err(DarmError::Rejected(format!(
                "expected height {}, got {}",
                self.height() + 1,
                header.height
            )));
        }
        if header.prev != self.head_hash() {
            return Err(DarmError::Rejected("block does not extend the head".into()));
        }
        if header.validators != self.config.validators.commitment() {
            return Err(DarmError::Rejected("validator set mismatch".into()));
        }
        let previous_ts = self.blocks.last().map_or(0, |b| b.header.timestamp_ms);
        if header.timestamp_ms < previous_ts {
            return Err(DarmError::Rejected("block timestamp went backwards".into()));
        }
        if self
            .config
            .validators
            .proposer_for(header.height, header.round)
            != header.proposer
        {
            return Err(DarmError::Unauthorized("wrong proposer for round".into()));
        }
        header.proposer.verify(
            &block_proposal_message(&self.chain_id, &header.hash()),
            &block.proposer_signature,
        )?;
        if block.entries.is_empty() || block.entries.len() > self.limits.max_entries_per_block {
            return Err(DarmError::LimitExceeded("entry count out of range".into()));
        }
        if header.entry_count as usize != block.entries.len() {
            return Err(DarmError::malformed("block", "entry_count mismatch"));
        }
        let ids = block.entry_ids();
        if merkle::root(&ids) != header.entries_root {
            return Err(DarmError::malformed("block", "entries_root mismatch"));
        }
        let mut seen = HashSet::with_capacity(ids.len());
        let mut state = self.state.clone();
        for (entry, id) in block.entries.iter().zip(&ids) {
            if !seen.insert(*id) || self.committed_entries.contains_key(id) {
                return Err(DarmError::Rejected("duplicate entry in block".into()));
            }
            entry.verify(&self.chain_id)?;
            if entry.entry.timestamp_ms
                > header
                    .timestamp_ms
                    .saturating_add(self.limits.max_clock_skew_ms)
            {
                return Err(DarmError::Rejected("entry is newer than its block".into()));
            }
            state.apply(entry, header.timestamp_ms, &self.config.validators)?;
        }
        Ok(state)
    }

    fn verify_quorum(&self, block: &Block) -> Result<()> {
        let hash = block.hash();
        let message = attestation_message(&self.chain_id, block.header.height, &hash);
        let mut signers = HashSet::new();
        for attestation in &block.attestations {
            if !self.is_validator(&attestation.validator) {
                return Err(DarmError::Unauthorized(
                    "attestation from non-validator".into(),
                ));
            }
            attestation
                .validator
                .verify(&message, &attestation.signature)?;
            signers.insert(attestation.validator);
        }
        if !self.config.validators.has_quorum(signers.iter()) {
            return Err(DarmError::Rejected("block lacks attestation quorum".into()));
        }
        Ok(())
    }

    fn remove_pending(&mut self, key: &(u64, Hash32)) {
        if let Some(entry) = self.pending.remove(key) {
            self.pending_ids.remove(&key.1);
            if let Some(count) = self.pending_per_author.get_mut(&entry.entry.author) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.pending_per_author.remove(&entry.entry.author);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
