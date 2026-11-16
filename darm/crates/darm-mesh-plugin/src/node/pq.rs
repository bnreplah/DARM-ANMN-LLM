//! Post-quantum identity: bind this node's ML-DSA-65 key on the ledger and
//! keep the session layer's view of bound keys current.
//!
//! Once the chain activates the `pq-identity` feature, a registered node or
//! validator submits one `PqKeyBind` entry. A node whose chain schedules the
//! feature signs session bindings with both keys from the start, and it
//! requires both from every peer whose key the ledger has bound.

use super::DarmNode;
use crate::wire::{Outbound, Wire};
use darm_core::Result;
use darm_core::ledger::{EntryBody, Feature, pq_bind_message};

impl DarmNode {
    /// Tick step: submit this node's key binding once it is allowed.
    pub(super) fn ensure_pq_bound(&mut self, now_ms: u64) -> Result<Vec<Outbound>> {
        self.refresh_pq();
        let me = self.node_id();
        let next = self.ledger.height() + 1;
        if self.pq_bind_submitted
            || !self.ledger.config().active(Feature::PqIdentity, next)
            || self.ledger.state().pq_key(&me).is_some()
            || !(self.ledger.is_validator(&me) || self.ledger.state().registration(&me).is_some())
        {
            return Ok(Vec::new());
        }
        let pq = self.identity.pq_identity();
        let proof = pq.sign(
            "pq-key-bind",
            &pq_bind_message(&self.ledger.chain_id(), &me),
        );
        let entry = self.sign_entry(
            EntryBody::PqKeyBind {
                key: pq.public().as_bytes().to_vec(),
                proof,
            },
            now_ms,
        )?;
        self.ledger.submit(entry.clone(), now_ms)?;
        self.pq_bind_submitted = true;
        Ok(if self.ledger_gossip() {
            vec![Outbound::broadcast(Wire::LedgerEntry { entry })]
        } else {
            Vec::new()
        })
    }

    /// Bring the session layer in line with the ledger's bound keys.
    pub(super) fn refresh_pq(&mut self) {
        let bound = self.ledger.state().pq_keys();
        if bound.len() != self.sessions.pq_key_count() {
            // Keys are only ever added, so a changed count is a changed set.
            self.sessions.set_pq_keys(bound.clone());
        }
    }
}
