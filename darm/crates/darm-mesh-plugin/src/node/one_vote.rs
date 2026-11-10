//! The one-vote consensus rule in the node: block proposals and
//! attestations, at most one attestation per validator per height.
//!
//! This is the original rule and the default. Under `DARM_CONSENSUS=locking`
//! the node uses `rounds` instead and ignores these messages.

use super::{DarmNode, MAX_EARLY_BLOCKS, MAX_EARLY_PER_BLOCK};
use crate::wire::{Outbound, Wire};
use darm_core::ledger::{Attestation, Block};
use darm_core::{Hash32, Result};

impl DarmNode {
    /// Route a consensus message to the rule this node runs.
    pub(super) fn on_consensus_message(
        &mut self,
        source: &str,
        message: Wire,
        ledger_gossip: bool,
        now_ms: u64,
    ) -> Result<Vec<Outbound>> {
        if self.locking() {
            return self.on_round_message(source, message, ledger_gossip, now_ms);
        }
        match message {
            Wire::BlockProposal { block } if ledger_gossip => {
                self.on_block_proposal(source, block, now_ms)
            }
            Wire::Attestation {
                block_hash,
                attestation,
            } if ledger_gossip => {
                if self.ledger.candidate(&block_hash).is_some() {
                    self.ledger.receive_attestation(&block_hash, attestation)?;
                    self.sync_effects()?;
                } else {
                    self.buffer_attestation(block_hash, attestation);
                }
                Ok(Vec::new())
            }
            // Round messages mean nothing under the one-vote rule.
            _ => Ok(Vec::new()),
        }
    }

    fn on_block_proposal(
        &mut self,
        source: &str,
        block: Block,
        now_ms: u64,
    ) -> Result<Vec<Outbound>> {
        if block.header.height > self.ledger.height() + 1 {
            return Ok(vec![Outbound::to(
                source,
                Wire::SyncRequest {
                    chain: self.ledger.chain_id(),
                    from_height: self.ledger.height(),
                },
            )]);
        }
        let mut out = Vec::new();
        let (hash, _) = self.ledger.receive_proposal(block, now_ms)?;
        self.apply_early_attestations(&hash);
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
        Ok(out)
    }

    pub(super) fn try_propose_one_vote(&mut self, now_ms: u64) -> Result<Vec<Outbound>> {
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

    fn buffer_attestation(&mut self, block: Hash32, attestation: Attestation) {
        if !self.ledger.is_validator(&attestation.validator) {
            return;
        }
        if !self.early_attestations.contains_key(&block)
            && self.early_attestations.len() >= MAX_EARLY_BLOCKS
        {
            return;
        }
        let pending = self.early_attestations.entry(block).or_default();
        if pending.len() < MAX_EARLY_PER_BLOCK
            && !pending.iter().any(|a| a.validator == attestation.validator)
        {
            pending.push(attestation);
        }
    }

    /// The block for buffered attestations arrived: verify and apply them.
    fn apply_early_attestations(&mut self, block: &Hash32) {
        let Some(pending) = self.early_attestations.remove(block) else {
            return;
        };
        for attestation in pending {
            if self.ledger.candidate(block).is_none() {
                break;
            }
            if let Err(error) = self.ledger.receive_attestation(block, attestation) {
                tracing::debug!(%error, "darm buffered attestation rejected");
            }
        }
    }
}
