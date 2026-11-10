//! Lock-based BFT agreement on one ledger height.
//!
//! The base ledger rule (one attestation per height) is safe but a proposer
//! that sends different blocks to different validators can split the vote so
//! that no block ever reaches quorum, and nobody may vote again. This module
//! fixes that with Tendermint-style rounds and locks.
//!
//! Each height runs rounds `0, 1, 2, ...`; round `r` has one proposer. A
//! round has two votes:
//!
//! 1. **Prevote**: "this is the block I would accept in this round", or nil.
//! 2. **Precommit**: cast for a block only after seeing prevotes for it from
//!    more than two thirds of the weight in the same round (a *polka*);
//!    otherwise nil.
//!
//! A block is decided by precommits from more than two thirds of the weight
//! in one round. When a validator precommits a block it **locks** on it:
//! in later rounds it prevotes nothing else, unless it sees a polka for
//! another block from a round newer than its lock. A proposer that knows of
//! a polka re-proposes that block (naming the polka's round) instead of a
//! fresh one, which is what lets locked validators converge.
//!
//! Safety (two honest validators never decide different blocks at a height)
//! holds with less than one third of the weight faulty and does not depend
//! on timing: timeouts only move rounds forward. Liveness needs the network
//! to deliver messages within the caller's timeouts eventually.
//!
//! [`HeightVote`] is one validator's (or an observer's) state for a height.
//! It performs no I/O and reads no clock. The caller feeds it proposals,
//! votes and timeouts, and broadcasts the votes it returns.
//!
//! Two certificates travel between nodes and are verified independently of
//! the receiver's own vote table: a proposer that re-proposes a block sends
//! its polka ([`HeightVote::polka`], accepted by [`HeightVote::on_polka`]),
//! and a node that decides relays its commit certificate
//! ([`Action::Commit`], accepted by [`HeightVote::on_commit`]). Without
//! them a Byzantine validator could show a polka or a decision to some
//! nodes and withhold it from others for good.
//!
//! **Durability requirement**: a validator that forgets its lock or the
//! votes it cast can be made to vote twice after a restart. Whatever wires
//! this in must persist every vote from [`Action::Broadcast`] before
//! sending it, and replay them through [`HeightVote::on_vote`] on restart.

use super::validators::ValidatorSet;
use crate::canonical::Canonical;
use crate::{DarmError, Hash32, NodeId, NodeIdentity, Result, Signature};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Rounds ahead of the current one for which messages are kept. Bounds the
/// memory a peer can claim by voting in far-future rounds.
pub const MAX_ROUND_LOOKAHEAD: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteKind {
    Prevote,
    Precommit,
}

/// A signed prevote or precommit. `block: None` is a vote for nil.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vote {
    pub kind: VoteKind,
    pub height: u64,
    pub round: u32,
    pub block: Option<Hash32>,
    pub validator: NodeId,
    pub signature: Signature,
}

pub fn vote_message(
    chain: &Hash32,
    kind: VoteKind,
    height: u64,
    round: u32,
    block: Option<&Hash32>,
) -> Vec<u8> {
    let mut encoder = Canonical::new("consensus-vote");
    encoder
        .fixed32(chain.as_bytes())
        .u8(match kind {
            VoteKind::Prevote => 1,
            VoteKind::Precommit => 2,
        })
        .u64(height)
        .u32(round)
        .opt_fixed32(block.map(Hash32::as_bytes));
    encoder.finish()
}

/// The proposer of `round` names the block for that round. `valid_round` is
/// set when the block is re-proposed because it had a polka in that earlier
/// round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub height: u64,
    pub round: u32,
    pub block: Hash32,
    pub valid_round: Option<u32>,
    pub proposer: NodeId,
    pub signature: Signature,
}

pub fn proposal_message(
    chain: &Hash32,
    height: u64,
    round: u32,
    block: &Hash32,
    valid_round: Option<u32>,
) -> Vec<u8> {
    let mut encoder = Canonical::new("consensus-proposal");
    encoder
        .fixed32(chain.as_bytes())
        .u64(height)
        .u32(round)
        .fixed32(block.as_bytes())
        .bool(valid_round.is_some())
        .u32(valid_round.unwrap_or(0));
    encoder.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Propose,
    Prevote,
    Precommit,
}

/// What the caller must do after feeding the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Persist, then send to every validator.
    Broadcast(Vote),
    /// The height is decided. `precommits` is the quorum certificate.
    Commit {
        block: Hash32,
        round: u32,
        precommits: Vec<Vote>,
    },
    /// A validator signed two different votes of the same kind in one round.
    /// Both are valid signatures, so the pair is proof of misbehaviour.
    Equivocation(Box<(Vote, Vote)>),
    /// The round advanced; restart the caller's timers.
    EnterRound(u32),
}

#[derive(Debug)]
pub struct HeightVote {
    chain: Hash32,
    height: u64,
    validators: ValidatorSet,
    round: u32,
    step: Step,
    locked: Option<(u32, Hash32)>,
    valid: Option<(u32, Hash32)>,
    proposals: HashMap<u32, Proposal>,
    /// Blocks whose bodies the caller has validated against ledger state.
    acceptable: HashSet<Hash32>,
    votes: BTreeMap<(u32, VoteKind), HashMap<NodeId, Vote>>,
    /// Verified polkas by `(round, block)`, kept so they can be relayed.
    polkas: HashMap<(u32, Hash32), Vec<Vote>>,
    decided: Option<Hash32>,
    /// The round and precommits that decided the height.
    certificate: Option<(u32, Vec<Vote>)>,
}

impl HeightVote {
    pub fn new(chain: Hash32, height: u64, validators: ValidatorSet) -> Self {
        Self {
            chain,
            height,
            validators,
            round: 0,
            step: Step::Propose,
            locked: None,
            valid: None,
            proposals: HashMap::new(),
            acceptable: HashSet::new(),
            votes: BTreeMap::new(),
            polkas: HashMap::new(),
            decided: None,
            certificate: None,
        }
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn round(&self) -> u32 {
        self.round
    }

    pub fn step(&self) -> Step {
        self.step
    }

    /// The `(round, block)` this validator is locked on, if any.
    pub fn locked(&self) -> Option<(u32, Hash32)> {
        self.locked
    }

    /// The newest `(round, block)` known to have had a polka. A proposer
    /// must re-propose this block instead of building a new one.
    pub fn valid(&self) -> Option<(u32, Hash32)> {
        self.valid
    }

    pub fn decided(&self) -> Option<Hash32> {
        self.decided
    }

    /// The decided block with the round and precommits that decided it.
    /// Kept so a node that learns the decision before it has the block
    /// body can still finalize once the body arrives.
    pub fn commit_certificate(&self) -> Option<(Hash32, u32, &[Vote])> {
        let (round, precommits) = self.certificate.as_ref()?;
        Some((self.decided?, *round, precommits))
    }

    pub fn proposer(&self, round: u32) -> NodeId {
        self.validators.proposer_for(self.height, round)
    }

    /// Sign the proposal for the current round. `fresh` is the hash of a
    /// newly built block; it is used only when no block has had a polka.
    pub fn propose(&self, identity: &NodeIdentity, fresh: Hash32) -> Result<Proposal> {
        let proposer = identity.node_id();
        if self.proposer(self.round) != proposer {
            return Err(DarmError::Unauthorized(
                "not the proposer for this round".into(),
            ));
        }
        let (valid_round, block) = match self.valid {
            Some((round, block)) => (Some(round), block),
            None => (None, fresh),
        };
        let message = proposal_message(&self.chain, self.height, self.round, &block, valid_round);
        Ok(Proposal {
            height: self.height,
            round: self.round,
            block,
            valid_round,
            proposer,
            signature: identity.sign(&message),
        })
    }

    /// Everything about a proposal that can be checked without the block:
    /// height, round bound, proposer and signature. Callers use this before
    /// spending memory on the block body.
    pub fn check_proposal(&self, proposal: &Proposal) -> Result<()> {
        if proposal.height != self.height {
            return Err(DarmError::Rejected("proposal for another height".into()));
        }
        self.check_round(proposal.round)?;
        if proposal.proposer != self.proposer(proposal.round) {
            return Err(DarmError::Unauthorized(
                "not the proposer for that round".into(),
            ));
        }
        if proposal.valid_round.is_some_and(|vr| vr >= proposal.round) {
            return Err(DarmError::malformed(
                "proposal",
                "valid round not in the past",
            ));
        }
        let message = proposal_message(
            &self.chain,
            self.height,
            proposal.round,
            &proposal.block,
            proposal.valid_round,
        );
        proposal.proposer.verify(&message, &proposal.signature)?;
        Ok(())
    }

    /// The proposal recorded for `round`, if any.
    pub fn proposal(&self, round: u32) -> Option<&Proposal> {
        self.proposals.get(&round)
    }

    /// Whether any proposal or vote has been seen for this height. An idle
    /// chain has nothing to agree on, so callers start round timers only
    /// once this is true or they have something to propose.
    pub fn active(&self) -> bool {
        !self.proposals.is_empty() || !self.votes.is_empty()
    }

    /// Replay one of this node's own persisted votes after a restart.
    ///
    /// Besides recording the vote (so it is never signed again), this
    /// restores what casting it implied: a precommit for a block means the
    /// node was locked on it, and having voted in a round means the node
    /// had reached at least that step of that round. Without this a
    /// restarted validator would forget its lock and could help finalize a
    /// conflicting block.
    pub fn restore_own_vote(&mut self, vote: Vote) -> Result<()> {
        let (round, kind, block) = (vote.round, vote.kind, vote.block);
        if round > self.round.saturating_add(MAX_ROUND_LOOKAHEAD) {
            // The node itself was in this round; trust its own durable log
            // over the lookahead bound, which exists for peers.
            self.round = round;
            self.step = Step::Propose;
        }
        self.on_vote(None, vote)?;
        if self.decided.is_some() {
            return Ok(());
        }
        let reached = match kind {
            VoteKind::Prevote => Step::Prevote,
            VoteKind::Precommit => Step::Precommit,
        };
        if round > self.round || (round == self.round && reached > self.step) {
            self.round = round;
            self.step = reached;
        }
        if let (VoteKind::Precommit, Some(block)) = (kind, block)
            && self
                .locked
                .is_none_or(|(locked_round, _)| round >= locked_round)
        {
            self.locked = Some((round, block));
            self.acceptable.insert(block);
        }
        Ok(())
    }

    /// Record a proposal. `acceptable` says whether the caller validated the
    /// block body against ledger state; an unacceptable block gets a nil
    /// prevote. Pass `identity` to vote, `None` to only observe.
    pub fn on_proposal(
        &mut self,
        identity: Option<&NodeIdentity>,
        proposal: Proposal,
        acceptable: bool,
    ) -> Result<Vec<Action>> {
        self.check_proposal(&proposal)?;
        if let Some(existing) = self.proposals.get(&proposal.round) {
            if *existing == proposal {
                return Ok(Vec::new());
            }
            // The proposer equivocated. Only the first proposal counts here;
            // validators that saw the other one simply will not form a polka
            // with us, and the round times out.
            return Err(DarmError::Rejected(
                "conflicting proposal for this round".into(),
            ));
        }
        if acceptable {
            self.acceptable.insert(proposal.block);
        }
        self.proposals.insert(proposal.round, proposal);
        let mut actions = Vec::new();
        self.evaluate(identity, &mut actions);
        Ok(actions)
    }

    /// Record a vote from any validator, including this node's own votes
    /// when replaying after a restart.
    pub fn on_vote(&mut self, identity: Option<&NodeIdentity>, vote: Vote) -> Result<Vec<Action>> {
        if vote.height != self.height {
            return Err(DarmError::Rejected("vote for another height".into()));
        }
        self.check_round(vote.round)?;
        if !self.validators.contains(&vote.validator) {
            return Err(DarmError::Unauthorized("vote from non-validator".into()));
        }
        let message = vote_message(
            &self.chain,
            vote.kind,
            vote.height,
            vote.round,
            vote.block.as_ref(),
        );
        vote.validator.verify(&message, &vote.signature)?;
        let mut actions = Vec::new();
        let slot = self.votes.entry((vote.round, vote.kind)).or_default();
        match slot.get(&vote.validator) {
            Some(existing) if existing.block == vote.block => return Ok(actions),
            Some(existing) => {
                actions.push(Action::Equivocation(Box::new((existing.clone(), vote))));
                return Ok(actions);
            }
            None => {
                slot.insert(vote.validator, vote);
            }
        }
        self.evaluate(identity, &mut actions);
        Ok(actions)
    }

    /// The caller's timer for `(round, step)` fired. Stale timers are
    /// ignored, so they never need cancelling.
    pub fn on_timeout(
        &mut self,
        identity: Option<&NodeIdentity>,
        round: u32,
        step: Step,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.decided.is_some() || round != self.round || step != self.step {
            return actions;
        }
        match step {
            Step::Propose => {
                self.cast(identity, VoteKind::Prevote, None, &mut actions);
                self.step = Step::Prevote;
            }
            Step::Prevote => {
                self.cast(identity, VoteKind::Precommit, None, &mut actions);
                self.step = Step::Precommit;
            }
            Step::Precommit => self.enter_round(round.saturating_add(1), &mut actions),
        }
        self.evaluate(identity, &mut actions);
        actions
    }

    /// Accept a decision reached elsewhere: a set of precommits for one
    /// block in one round carrying more than two thirds of the weight.
    ///
    /// The certificate is checked on its own, not merged into this node's
    /// vote table. That matters: a Byzantine validator can show this node a
    /// different precommit than the one it sent the nodes that decided, and
    /// a node that only counted first-seen votes could then never catch up.
    /// Nodes that decide should relay their certificate.
    pub fn on_commit(&mut self, block: Hash32, round: u32, precommits: &[Vote]) -> Result<()> {
        self.verify_certificate(VoteKind::Precommit, block, round, precommits)?;
        match self.decided {
            Some(decided) if decided != block => Err(DarmError::Rejected(
                "conflicting commit certificate: more than a third of the weight is faulty".into(),
            )),
            _ => {
                self.decided = Some(block);
                self.certificate
                    .get_or_insert_with(|| (round, precommits.to_vec()));
                Ok(())
            }
        }
    }

    /// Accept a polka seen elsewhere: prevotes for one block in one round
    /// carrying more than two thirds of the weight. A proposer that
    /// re-proposes a block sends this alongside, so validators locked on an
    /// older block can verify the newer polka instead of having to have
    /// collected the same votes themselves. Like [`Self::on_commit`], the
    /// certificate is checked on its own, because a Byzantine validator may
    /// have shown this node a different prevote.
    pub fn on_polka(
        &mut self,
        identity: Option<&NodeIdentity>,
        block: Hash32,
        round: u32,
        prevotes: &[Vote],
    ) -> Result<Vec<Action>> {
        self.check_round(round)?;
        let mut actions = Vec::new();
        if self.polkas.contains_key(&(round, block)) {
            return Ok(actions);
        }
        self.verify_certificate(VoteKind::Prevote, block, round, prevotes)?;
        self.polkas.insert((round, block), prevotes.to_vec());
        self.evaluate(identity, &mut actions);
        Ok(actions)
    }

    /// The polka certificate for `(round, block)`, if this node has one.
    /// Send it with any proposal whose `valid_round` is `round`.
    pub fn polka(&self, round: u32, block: &Hash32) -> Option<&[Vote]> {
        self.polkas.get(&(round, *block)).map(Vec::as_slice)
    }

    fn verify_certificate(
        &self,
        kind: VoteKind,
        block: Hash32,
        round: u32,
        votes: &[Vote],
    ) -> Result<()> {
        if votes.len() > self.validators.len() {
            return Err(DarmError::LimitExceeded("certificate".into()));
        }
        let message = vote_message(&self.chain, kind, self.height, round, Some(&block));
        for vote in votes {
            if vote.kind != kind
                || vote.height != self.height
                || vote.round != round
                || vote.block != Some(block)
            {
                return Err(DarmError::malformed(
                    "certificate",
                    "vote does not match the certificate",
                ));
            }
            if !self.validators.contains(&vote.validator) {
                return Err(DarmError::Unauthorized("vote from non-validator".into()));
            }
            vote.validator.verify(&message, &vote.signature)?;
        }
        // `has_quorum` counts each validator once, so repeats add nothing.
        if !self
            .validators
            .has_quorum(votes.iter().map(|v| &v.validator))
        {
            return Err(DarmError::Rejected("certificate lacks a quorum".into()));
        }
        Ok(())
    }

    fn check_round(&self, round: u32) -> Result<()> {
        if round > self.round.saturating_add(MAX_ROUND_LOOKAHEAD) {
            return Err(DarmError::LimitExceeded("round too far ahead".into()));
        }
        Ok(())
    }

    fn enter_round(&mut self, round: u32, actions: &mut Vec<Action>) {
        self.round = round;
        self.step = Step::Propose;
        // Rounds this node skipped can no longer matter to it except as the
        // source of a polka or a decision, both of which are kept.
        actions.push(Action::EnterRound(round));
    }

    /// Weight-quorum check over the validators that voted `block` in
    /// `(round, kind)`.
    fn quorum(&self, round: u32, kind: VoteKind, block: Option<&Hash32>) -> bool {
        self.votes.get(&(round, kind)).is_some_and(|slot| {
            self.validators.has_quorum(
                slot.values()
                    .filter(|v| v.block.as_ref() == block)
                    .map(|v| &v.validator),
            )
        })
    }

    /// The block, if any, with a quorum in `(round, kind)`. `Some(None)` is
    /// a quorum for nil.
    fn quorum_block(&self, round: u32, kind: VoteKind) -> Option<Option<Hash32>> {
        let slot = self.votes.get(&(round, kind))?;
        let mut seen: Vec<Option<Hash32>> = slot.values().map(|v| v.block).collect();
        seen.sort();
        seen.dedup();
        seen.into_iter()
            .find(|block| self.quorum(round, kind, block.as_ref()))
    }

    /// Sign and record this node's vote for the current round, once.
    fn cast(
        &mut self,
        identity: Option<&NodeIdentity>,
        kind: VoteKind,
        block: Option<Hash32>,
        actions: &mut Vec<Action>,
    ) {
        let Some(identity) = identity else { return };
        let validator = identity.node_id();
        if !self.validators.contains(&validator) {
            return;
        }
        let slot = self.votes.entry((self.round, kind)).or_default();
        if slot.contains_key(&validator) {
            // Already voted in this round (for example replayed after a
            // restart). Never sign a second vote.
            return;
        }
        let message = vote_message(&self.chain, kind, self.height, self.round, block.as_ref());
        let vote = Vote {
            kind,
            height: self.height,
            round: self.round,
            block,
            validator,
            signature: identity.sign(&message),
        };
        slot.insert(validator, vote.clone());
        actions.push(Action::Broadcast(vote));
    }

    /// The prevote the locking rules allow for the current round's
    /// proposal, or `None` when the decision must wait for more votes.
    fn prevote_choice(&self) -> Option<Option<Hash32>> {
        let proposal = self.proposals.get(&self.round)?;
        let block = proposal.block;
        if !self.acceptable.contains(&block) {
            return Some(None);
        }
        let free = match (self.locked, proposal.valid_round) {
            (None, None) => true,
            (Some((_, locked)), None) => locked == block,
            (lock, Some(valid_round)) => {
                if !self.polkas.contains_key(&(valid_round, block)) {
                    // The polka the proposer claims has not reached us yet.
                    return None;
                }
                lock.is_none_or(|(round, locked)| locked == block || round <= valid_round)
            }
        };
        Some(free.then_some(block))
    }

    /// Apply every rule that the current messages enable, until nothing
    /// changes.
    fn evaluate(&mut self, identity: Option<&NodeIdentity>, actions: &mut Vec<Action>) {
        // Each pass either decides, changes step, or enters a later round,
        // and rounds are bounded by the lookahead, so this terminates.
        while self.decided.is_none() && self.advance(identity, actions) {}
    }

    fn advance(&mut self, identity: Option<&NodeIdentity>, actions: &mut Vec<Action>) -> bool {
        if let Some((round, block)) = self.commit_quorum() {
            let precommits: Vec<Vote> = self.votes[&(round, VoteKind::Precommit)]
                .values()
                .filter(|v| v.block == Some(block))
                .cloned()
                .collect();
            self.decided = Some(block);
            self.certificate = Some((round, precommits.clone()));
            actions.push(Action::Commit {
                block,
                round,
                precommits,
            });
            return true;
        }
        if let Some(round) = self.round_to_skip_to() {
            self.enter_round(round, actions);
            return true;
        }
        if self.record_local_polka() {
            return true;
        }
        if self.step == Step::Propose
            && let Some(choice) = self.prevote_choice()
        {
            self.cast(identity, VoteKind::Prevote, choice, actions);
            self.step = Step::Prevote;
            return true;
        }
        match self.quorum_block(self.round, VoteKind::Prevote) {
            // A polka only counts here once the block itself is known and
            // validated: a validator must never lock on, or ask a proposer
            // to re-propose, a block it cannot check.
            Some(Some(block)) if self.step >= Step::Prevote && self.acceptable.contains(&block) => {
                let newer = self.valid != Some((self.round, block));
                self.valid = Some((self.round, block));
                if self.step == Step::Prevote {
                    self.locked = Some((self.round, block));
                    self.cast(identity, VoteKind::Precommit, Some(block), actions);
                    self.step = Step::Precommit;
                    return true;
                }
                newer
            }
            Some(None) if self.step == Step::Prevote => {
                self.cast(identity, VoteKind::Precommit, None, actions);
                self.step = Step::Precommit;
                true
            }
            _ => false,
        }
    }

    /// Turn a prevote quorum in this node's own vote table into a stored
    /// polka certificate. Returns whether a new one was recorded.
    fn record_local_polka(&mut self) -> bool {
        let found = self
            .votes
            .keys()
            .filter(|(_, kind)| *kind == VoteKind::Prevote)
            .find_map(|(round, kind)| {
                let block = self.quorum_block(*round, *kind)??;
                (!self.polkas.contains_key(&(*round, block))).then_some((*round, block))
            });
        let Some((round, block)) = found else {
            return false;
        };
        let certificate = self.votes[&(round, VoteKind::Prevote)]
            .values()
            .filter(|v| v.block == Some(block))
            .cloned()
            .collect();
        self.polkas.insert((round, block), certificate);
        true
    }

    /// A round in which some block has a precommit quorum.
    fn commit_quorum(&self) -> Option<(u32, Hash32)> {
        self.votes
            .keys()
            .filter(|(_, kind)| *kind == VoteKind::Precommit)
            .find_map(|(round, kind)| Some((*round, self.quorum_block(*round, *kind)??)))
    }

    /// The newest later round in which more than one third of the weight
    /// has voted. At least one of those validators is honest, so the
    /// network has moved on and this node should follow.
    fn round_to_skip_to(&self) -> Option<u32> {
        let total = self.validators.total_weight();
        let mut voters: BTreeMap<u32, HashSet<NodeId>> = BTreeMap::new();
        for ((round, _), slot) in self
            .votes
            .range((self.round.saturating_add(1), VoteKind::Prevote)..)
        {
            voters.entry(*round).or_default().extend(slot.keys());
        }
        voters.into_iter().rev().find_map(|(round, nodes)| {
            let weight: u64 = nodes.iter().map(|n| self.validators.weight(n)).sum();
            (weight * 3 > total).then_some(round)
        })
    }
}

#[cfg(test)]
mod tests;
