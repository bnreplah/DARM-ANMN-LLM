//! Lock-based BFT in the node: gossip, durability and timers around
//! `darm_core::ledger::locking`.
//!
//! The ledger decides; this module moves its messages and keeps its two
//! promises to the protocol. Every vote is written to the vote log before
//! it is sent, so a restart cannot make this validator sign twice or forget
//! a lock. And timers only run while there is something to agree on, so an
//! idle chain does not burn through rounds.

use super::{DarmNode, ROUND_TIMEOUT_MS};
use crate::config::ConsensusMode;
use crate::wire::{Outbound, Wire};
use darm_core::Result;
use darm_core::ledger::locking::{Action, Vote};
use darm_core::ledger::{EntryBody, Feature, RoundProposal};

impl DarmNode {
    pub(super) fn locking(&self) -> bool {
        self.config.consensus == ConsensusMode::Locking
    }

    /// Enable the configured consensus rule and, under locking, replay this
    /// validator's persisted votes for the open height.
    pub(super) fn start_consensus(&mut self) -> Result<()> {
        if !self.locking() {
            return Ok(());
        }
        self.ledger.enable_locking();
        let height = self.ledger.height() + 1;
        match &self.vote_log {
            Some(log) => {
                for vote in log.load(height)? {
                    // Only this node's own votes belong in its log.
                    if vote.validator == self.node_id() {
                        self.ledger.restore_round_vote(vote)?;
                    }
                }
            }
            None if self.ledger.is_validator(&self.node_id()) => {
                tracing::warn!(
                    "darm validator runs lock-based BFT without a data directory: \
                     votes are not durable, so do not restart it mid-height"
                );
            }
            None => {}
        }
        Ok(())
    }

    /// Whether this node may vote (it is a validator) or only observes.
    fn votes(&self) -> bool {
        self.ledger.is_validator(&self.identity.node_id())
    }

    pub(super) fn try_propose_round(&mut self, now_ms: u64) -> Result<Vec<Outbound>> {
        if self.ledger.pending_len() == 0 && !self.round_in_progress() {
            return Ok(Vec::new());
        }
        if !self.votes() {
            return Ok(Vec::new());
        }
        let Some((message, actions)) = self.ledger.propose_round(&self.identity, now_ms)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        if self.ledger_gossip() {
            out.push(Outbound::broadcast(Wire::RoundProposal {
                message: Box::new(message),
            }));
        }
        out.append(&mut self.after_round_actions(actions, now_ms)?);
        Ok(out)
    }

    fn round_in_progress(&self) -> bool {
        self.ledger.locking().is_some_and(|rounds| rounds.active())
    }

    /// Ask `source` for blocks when a message is for a height past ours.
    fn behind(&self, source: &str, height: u64) -> Option<Outbound> {
        (height > self.ledger.height() + 1).then(|| {
            Outbound::to(
                source,
                Wire::SyncRequest {
                    chain: self.ledger.chain_id(),
                    from_height: self.ledger.height(),
                },
            )
        })
    }

    /// Route one consensus message. One-vote messages are dropped: this
    /// node neither stores their blocks nor counts their signatures.
    pub(super) fn on_round_message(
        &mut self,
        source: &str,
        message: Wire,
        ledger_gossip: bool,
        now_ms: u64,
    ) -> Result<Vec<Outbound>> {
        match message {
            Wire::RoundProposal { message } if ledger_gossip => {
                self.on_round_proposal(source, *message, now_ms)
            }
            Wire::RoundVote { vote } if ledger_gossip => self.on_round_vote(source, vote, now_ms),
            _ => Ok(Vec::new()),
        }
    }

    fn on_round_proposal(
        &mut self,
        source: &str,
        message: RoundProposal,
        now_ms: u64,
    ) -> Result<Vec<Outbound>> {
        if let Some(request) = self.behind(source, message.proposal.height) {
            return Ok(vec![request]);
        }
        let voter = self.votes().then_some(&self.identity);
        let actions = self.ledger.on_round_proposal(voter, message, now_ms)?;
        self.after_round_actions(actions, now_ms)
    }

    fn on_round_vote(&mut self, source: &str, vote: Vote, now_ms: u64) -> Result<Vec<Outbound>> {
        if let Some(request) = self.behind(source, vote.height) {
            return Ok(vec![request]);
        }
        let voter = self.votes().then_some(&self.identity);
        let actions = self.ledger.on_round_vote(voter, vote)?;
        self.after_round_actions(actions, now_ms)
    }

    /// Fire the timer for the current `(height, round, step)` when it has
    /// run long enough. Later rounds wait longer, so a slow network
    /// eventually gets a round in which messages arrive in time.
    pub(super) fn tick_round_timer(&mut self, now_ms: u64) -> Result<Vec<Outbound>> {
        let Some(rounds) = self.ledger.locking() else {
            return Ok(Vec::new());
        };
        let key = (rounds.height(), rounds.round(), rounds.step());
        if rounds.decided().is_some() || (self.ledger.pending_len() == 0 && !rounds.active()) {
            self.round_timer = None;
            return Ok(Vec::new());
        }
        let started = match self.round_timer {
            Some((timed, started)) if timed == key => started,
            _ => {
                self.round_timer = Some((key, now_ms));
                return Ok(Vec::new());
            }
        };
        let wait = ROUND_TIMEOUT_MS.saturating_mul(u64::from(key.1) + 1);
        if now_ms.saturating_sub(started) <= wait {
            return Ok(Vec::new());
        }
        let voter = self.votes().then_some(&self.identity);
        let actions = self.ledger.on_round_timeout(voter, key.1, key.2)?;
        self.round_timer = None;
        self.after_round_actions(actions, now_ms)
    }

    /// Put equivocation evidence on the ledger once the chain has activated
    /// the `evidence` feature. Failures (duplicate evidence, feature not
    /// active, a peer already reported it) are expected and only logged.
    fn report_equivocation(&mut self, pair: (Vote, Vote), now_ms: u64) -> Vec<Outbound> {
        let next = self.ledger.height() + 1;
        if !self.ledger.config().active(Feature::Evidence, next) {
            return Vec::new();
        }
        let (first, second) = if pair.0.block < pair.1.block {
            pair
        } else {
            (pair.1, pair.0)
        };
        let body = EntryBody::Equivocation { first, second };
        let entry = match self.sign_entry(body, now_ms) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(%error, "darm could not sign equivocation evidence");
                return Vec::new();
            }
        };
        match self.ledger.submit(entry.clone(), now_ms) {
            Ok(true) if self.ledger_gossip() => {
                vec![Outbound::broadcast(Wire::LedgerEntry { entry })]
            }
            Ok(_) => Vec::new(),
            Err(error) => {
                tracing::debug!(%error, "darm equivocation evidence not submitted");
                Vec::new()
            }
        }
    }

    /// Carry out what the ledger asked for. Votes are persisted before they
    /// are queued for sending; a vote that cannot be persisted is not sent.
    fn after_round_actions(&mut self, actions: Vec<Action>, now_ms: u64) -> Result<Vec<Outbound>> {
        let gossip = self.ledger_gossip();
        let mut out = Vec::new();
        for action in actions {
            match action {
                Action::Broadcast(vote) => {
                    if let Some(log) = &self.vote_log {
                        log.append(&vote)?;
                    }
                    if gossip {
                        out.push(Outbound::broadcast(Wire::RoundVote { vote }));
                    }
                }
                Action::Equivocation(pair) => {
                    self.counters.equivocations += 1;
                    tracing::warn!(
                        validator = %pair.0.validator.short(),
                        round = pair.0.round,
                        "darm validator signed two different votes in one round"
                    );
                    out.extend(self.report_equivocation(*pair, now_ms));
                }
                Action::EnterRound(_) => self.round_timer = None,
                Action::Commit { .. } => {}
            }
        }
        let before = self.persisted_height;
        self.sync_effects()?;
        if self.persisted_height > before {
            if let Some(log) = &self.vote_log {
                log.clear()?;
            }
            self.round_timer = None;
            // Relay the finalized block with its certificate: a peer that
            // was shown different votes verifies it on its own.
            if gossip && let Some(block) = self.ledger.block(self.persisted_height) {
                out.push(Outbound::broadcast(Wire::SyncBlocks {
                    blocks: vec![block.clone()],
                }));
            }
        }
        Ok(out)
    }
}
