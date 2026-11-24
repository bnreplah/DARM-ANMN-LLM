//! Archive state: a verifying, non-voting replica of one DARM-ANN chain plus a
//! store of mesh-scoped memories. Nothing is accepted on trust: blocks must
//! carry a validator quorum, memories and tombstones must carry valid author
//! signatures. The HTTP write token only rate-gates who may submit.

use darm_core::ledger::store::BlockLog;
use darm_core::ledger::{Block, ChainConfig, Feature, Ledger, ProposalOutcome, ValidatorSet};
use darm_core::{DarmError, NodeId, Result};
use darm_memory::{MemoryLimits, MemoryLog, MemoryStore};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct ArchiveConfig {
    pub data_dir: Option<PathBuf>,
    pub chain_name: String,
    pub validators: Vec<NodeId>,
    /// Feature activation heights; must match the mesh validators'
    /// `DARM_ACTIVATE` or this archive rejects their blocks.
    pub activations: BTreeMap<Feature, u64>,
}

/// Parse `feature@height` items (the mesh's `DARM_ACTIVATE` format).
pub fn parse_activations(items: &[String]) -> Result<BTreeMap<Feature, u64>> {
    let mut out = BTreeMap::new();
    for item in items.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let bad = || DarmError::Rejected(format!("activation {item:?} must be feature@height"));
        let (name, height) = item.split_once('@').ok_or_else(bad)?;
        let feature = Feature::parse(name).ok_or_else(bad)?;
        let height = height.trim().parse().map_err(|_| bad())?;
        if out.insert(feature, height).is_some() {
            return Err(bad());
        }
    }
    Ok(out)
}

pub struct Archive {
    pub ledger: Ledger,
    pub memory: MemoryStore,
    block_log: Option<BlockLog>,
}

impl Archive {
    pub fn open(config: ArchiveConfig) -> Result<Self> {
        let mut chain =
            ChainConfig::new(config.chain_name, ValidatorSet::equal(config.validators)?);
        chain.activations = config.activations;
        match config.data_dir {
            Some(dir) => {
                std::fs::create_dir_all(&dir)?;
                let log =
                    BlockLog::new(dir.join(format!("ledger-{}.jsonl", chain.chain_id().short())));
                let ledger = log.load(chain)?;
                let memory = MemoryStore::open(
                    MemoryLog::new(dir.join("memory.jsonl")),
                    MemoryLimits::default(),
                )?;
                Ok(Self {
                    ledger,
                    memory,
                    block_log: Some(log),
                })
            }
            None => Ok(Self {
                ledger: Ledger::new(chain),
                memory: MemoryStore::new(MemoryLimits::default()),
                block_log: None,
            }),
        }
    }

    /// Append finalized blocks in order. Already-known heights are skipped;
    /// a gap or an invalid block stops the batch with an error.
    pub fn append_blocks(&mut self, blocks: Vec<Block>, now_ms: u64) -> Result<u64> {
        for block in blocks {
            if block.header.height <= self.ledger.height() {
                continue;
            }
            self.ledger.commit_finalized(block, Some(now_ms))?;
            let committed = self.ledger.blocks().last().expect("just committed");
            if let Some(log) = &self.block_log {
                log.append(committed)?;
            }
            for entry in &committed.entries {
                if let darm_core::ledger::EntryBody::MemoryCommit { memory, .. } = &entry.entry.body
                {
                    // The memory may not have been uploaded yet; that is fine.
                    let _ = self.memory.mark_finalized(memory, now_ms);
                }
            }
        }
        Ok(self.ledger.height())
    }

    pub fn passed_proposals(&self, kind: Option<&str>, now_ms: u64) -> Vec<serde_json::Value> {
        let validators = &self.ledger.config().validators;
        let mut out: Vec<_> = self
            .ledger
            .state()
            .proposals()
            .filter(|(_, p)| kind.is_none_or(|k| p.kind == k))
            .map(|(id, p)| {
                let outcome: ProposalOutcome = p.outcome(validators, now_ms);
                serde_json::json!({
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
        out.sort_by(|a, b| a["deadline_ms"].as_u64().cmp(&b["deadline_ms"].as_u64()));
        out
    }
}
