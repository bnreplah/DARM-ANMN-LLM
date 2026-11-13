//! Lock-based BFT on the ledger.
//!
//! [`super::super::locking::HeightVote`] decides which block hash wins a
//! height. This module joins it to the ledger: it supplies block bodies,
//! tells the round machine which blocks pass the ledger's own rules, and
//! turns a decision into a finalized block whose commit certificate is the
//! deciding round's precommits.

use super::Ledger;
use crate::ledger::entry::{Attestation, Block};
use crate::ledger::locking::{Action, HeightVote, Proposal, Step, Vote};
use crate::{DarmError, NodeIdentity, Result};
use serde::{Deserialize, Serialize};

/// What a round's proposer sends: the signed proposal, the block it names,
/// and, for a re-proposal, the polka that justifies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundProposal {
    pub proposal: Proposal,
    pub block: Block,
    #[serde(default)]
    pub polka: Vec<Vote>,
}

impl Ledger {
    /// Switch this ledger from the one-vote rule to lock-based BFT. All
    /// validators of a chain must switch together: a validator that signed
    /// under both rules at one height would count toward two quorums.
    pub fn enable_locking(&mut self) {
        if self.rounds.is_none() {
            self.start_height_vote();
        }
    }

    /// The agreement state for the next height, when locking is enabled.
    pub fn locking(&self) -> Option<&HeightVote> {
        self.rounds.as_ref()
    }

    pub(super) fn start_height_vote(&mut self) {
        self.rounds = Some(HeightVote::new(
            self.chain_id,
            self.height() + 1,
            self.validators().clone(),
        ));
    }

    fn rounds_mut(&mut self) -> Result<&mut HeightVote> {
        self.rounds
            .as_mut()
            .ok_or_else(|| DarmError::Rejected("lock-based BFT is not enabled".into()))
    }

    /// Propose for the current round if `identity` is its proposer and has
    /// not proposed yet. A block that already had a polka is proposed again
    /// with that polka; otherwise a fresh block is built from the pending
    /// pool. Returns `None` when there is nothing to do. The returned
    /// actions are this node's own reaction to its proposal (its prevote).
    pub fn propose_round(
        &mut self,
        identity: &NodeIdentity,
        now_ms: u64,
    ) -> Result<Option<(RoundProposal, Vec<Action>)>> {
        let rounds = self.rounds_mut()?;
        let round = rounds.round();
        if rounds.decided().is_some()
            || rounds.step() != Step::Propose
            || rounds.proposer(round) != identity.node_id()
            // Proposing twice in a round would be equivocation.
            || rounds.proposal(round).is_some()
        {
            return Ok(None);
        }
        let block = match rounds.valid() {
            Some((_, hash)) => self.candidates.get(&hash).cloned().ok_or_else(|| {
                DarmError::Rejected("block with a polka is not held locally".into())
            })?,
            None => match self.propose(identity, round, now_ms)? {
                Some(block) => block,
                None => return Ok(None),
            },
        };
        let hash = block.hash();
        let rounds = self.rounds_mut()?;
        let proposal = rounds.propose(identity, hash)?;
        let polka = proposal
            .valid_round
            .and_then(|valid_round| rounds.polka(valid_round, &hash))
            .map(<[Vote]>::to_vec)
            .unwrap_or_default();
        let message = RoundProposal {
            proposal,
            block,
            polka,
        };
        let actions = self.on_round_proposal(Some(identity), message.clone(), now_ms)?;
        Ok(Some((message, actions)))
    }

    /// Handle a proposal from the network (or this node's own). The block
    /// is validated against ledger state; one that fails is still recorded,
    /// so that this validator prevotes nil instead of staying silent.
    pub fn on_round_proposal(
        &mut self,
        identity: Option<&NodeIdentity>,
        message: RoundProposal,
        now_ms: u64,
    ) -> Result<Vec<Action>> {
        let RoundProposal {
            proposal,
            block,
            polka,
        } = message;
        let hash = block.hash();
        if hash != proposal.block {
            return Err(DarmError::malformed(
                "round proposal",
                "block does not match the proposal",
            ));
        }
        if block.header.round > proposal.round {
            return Err(DarmError::malformed(
                "round proposal",
                "block is from a later round than its proposal",
            ));
        }
        let rounds = self.rounds_mut()?;
        // Check the cheap, signed part first so an outsider cannot make
        // this node validate or store arbitrary block bodies.
        rounds.check_proposal(&proposal)?;
        if rounds
            .proposal(proposal.round)
            .is_some_and(|existing| *existing != proposal)
        {
            return Err(DarmError::Rejected(
                "conflicting proposal for this round".into(),
            ));
        }
        let acceptable = self.accept_candidate(block, now_ms);
        let rounds = self.rounds_mut()?;
        let mut actions = Vec::new();
        if let Some(valid_round) = proposal.valid_round
            && !polka.is_empty()
        {
            actions = rounds.on_polka(identity, hash, valid_round, &polka)?;
        }
        actions.extend(rounds.on_proposal(identity, proposal, acceptable)?);
        self.apply_round_actions(actions)
    }

    /// Whether `block` passes the ledger's rules and is held as a candidate.
    fn accept_candidate(&mut self, mut block: Block, now_ms: u64) -> bool {
        let hash = block.hash();
        if self.candidates.contains_key(&hash) {
            return true;
        }
        if block.header.timestamp_ms > now_ms.saturating_add(self.limits.max_clock_skew_ms) {
            return false;
        }
        // Certificates are built here from verified precommits, never taken
        // from a proposal.
        block.attestations.clear();
        block.commit_round = None;
        self.insert_candidate(block).is_ok()
    }

    pub fn on_round_vote(
        &mut self,
        identity: Option<&NodeIdentity>,
        vote: Vote,
    ) -> Result<Vec<Action>> {
        let actions = self.rounds_mut()?.on_vote(identity, vote)?;
        self.apply_round_actions(actions)
    }

    pub fn on_round_timeout(
        &mut self,
        identity: Option<&NodeIdentity>,
        round: u32,
        step: Step,
    ) -> Result<Vec<Action>> {
        let actions = self.rounds_mut()?.on_timeout(identity, round, step);
        self.apply_round_actions(actions)
    }

    /// Replay one of this node's own persisted votes after a restart.
    pub fn restore_round_vote(&mut self, vote: Vote) -> Result<()> {
        self.rounds_mut()?.restore_own_vote(vote)
    }

    /// Finalize the decided block once both the decision and the block body
    /// are held, in whichever order they arrived. The actions are returned
    /// unchanged so the caller can broadcast votes and react to round
    /// changes. A node that never receives the body gets the finalized
    /// block, certificate included, through sync.
    fn apply_round_actions(&mut self, actions: Vec<Action>) -> Result<Vec<Action>> {
        let decided = self
            .rounds
            .as_ref()
            .and_then(HeightVote::commit_certificate)
            .map(|(hash, round, precommits)| (hash, round, precommits.to_vec()));
        if let Some((hash, round, precommits)) = decided
            && let Some(mut block) = self.candidates.get(&hash).cloned()
        {
            block.commit_round = Some(round);
            block.attestations = precommits
                .iter()
                .map(|vote| Attestation {
                    validator: vote.validator,
                    signature: vote.signature,
                })
                .collect();
            self.commit_finalized(block, None)?;
        }
        Ok(actions)
    }
}

#[cfg(test)]
mod tests;
