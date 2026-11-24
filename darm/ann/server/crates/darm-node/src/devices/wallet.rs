//! A device's view of the credit ledger, derived from finalized blocks.
//!
//! Everything here is a function of the blocks, so a device can recompute it
//! from blocks it has verified itself and compare (the companion app does).
//! The invariant it relies on is the ledger's own (spec §4.7):
//!
//! `balance = granted + settled_in - settled_out`

use darm_core::ledger::{EntryBody, Ledger};
use darm_core::{Hash32, NodeId};
use serde_json::{Value, json};

/// Totals over the whole chain for one node.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Totals {
    /// Credits issued to the node by governance grants.
    pub granted: u64,
    /// Credits received as a worker in a compute settlement.
    pub settled_in: u64,
    /// Credits paid out as a coordinator.
    pub settled_out: u64,
    pub entries_authored: u64,
    pub settlements_received: u64,
}

fn kind_of(body: &EntryBody) -> String {
    serde_json::to_value(body)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

/// One entry that touches `node`, as the activity feed shows it.
struct Touch {
    role: &'static str,
    delta: i64,
    detail: Value,
}

fn touch(node: &NodeId, author: &NodeId, body: &EntryBody) -> Option<Touch> {
    let role = if author == node {
        "author"
    } else {
        "recipient"
    };
    match body {
        EntryBody::CreditGrant { to, amount, .. } if to == node => Some(Touch {
            role,
            delta: i64::try_from(*amount).unwrap_or(i64::MAX),
            detail: json!({"amount": amount, "to": to}),
        }),
        EntryBody::ComputeSettle {
            job,
            receipts_root,
            payments,
        } => {
            let total: u64 = payments
                .iter()
                .map(|p| p.amount)
                .fold(0, u64::saturating_add);
            let mine = payments
                .iter()
                .find(|p| &p.worker == node)
                .map(|p| p.amount);
            if author == node {
                Some(Touch {
                    role,
                    delta: -i64::try_from(total).unwrap_or(i64::MAX),
                    detail: json!({"job": job, "receipts_root": receipts_root, "paid": total, "payees": payments.len()}),
                })
            } else {
                mine.map(|amount| Touch {
                    role,
                    delta: i64::try_from(amount).unwrap_or(i64::MAX),
                    detail: json!({"job": job, "receipts_root": receipts_root, "amount": amount, "coordinator": author}),
                })
            }
        }
        _ if author == node => Some(Touch {
            role,
            delta: 0,
            detail: Value::Null,
        }),
        _ => None,
    }
}

/// Totals for `node` over every finalized block.
pub fn totals(ledger: &Ledger, node: &NodeId) -> Totals {
    let mut totals = Totals::default();
    for entry in ledger.blocks().iter().flat_map(|b| &b.entries) {
        let author = &entry.entry.author;
        if author == node {
            totals.entries_authored += 1;
        }
        match &entry.entry.body {
            EntryBody::CreditGrant { to, amount, .. } if to == node => {
                totals.granted = totals.granted.saturating_add(*amount);
            }
            EntryBody::ComputeSettle { payments, .. } => {
                for payment in payments {
                    if &payment.worker == node {
                        totals.settled_in = totals.settled_in.saturating_add(payment.amount);
                        totals.settlements_received += 1;
                    }
                }
                if author == node {
                    let paid: u64 = payments
                        .iter()
                        .map(|p| p.amount)
                        .fold(0, u64::saturating_add);
                    totals.settled_out = totals.settled_out.saturating_add(paid);
                }
            }
            _ => {}
        }
    }
    totals
}

/// Entries that touch `node`, newest first, below `before` (exclusive).
pub fn activity(ledger: &Ledger, node: &NodeId, before: Option<u64>, limit: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for block in ledger.blocks().iter().rev() {
        let height = block.header.height;
        if before.is_some_and(|b| height >= b) {
            continue;
        }
        let block_hash: Hash32 = block.header.hash();
        for entry in block.entries.iter().rev() {
            let Some(touch) = touch(node, &entry.entry.author, &entry.entry.body) else {
                continue;
            };
            out.push(json!({
                "height": height,
                "block": block_hash,
                "entry": entry.id(),
                "timestamp_ms": entry.entry.timestamp_ms,
                "kind": kind_of(&entry.entry.body),
                "role": touch.role,
                "delta": touch.delta,
                "detail": touch.detail,
            }));
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
}
