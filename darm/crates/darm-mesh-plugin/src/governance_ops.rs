//! Governance operations: draft, vote on and enact validator-set changes and
//! credit grants without hand-building ledger entries.
//!
//! Every step is still an ordinary signed ledger entry with the rules of the
//! ledger (two-thirds of the validator set in force, feature activation,
//! effective-height delay). These operations only build the entries, check
//! locally what the ledger will check, and explain refusals.

use crate::node::DarmNode;
use crate::{SharedNode, send_outbound};
use darm_core::ledger::{
    CREDIT_GRANT_KIND, EntryBody, Member, ProposalOutcome, VALIDATOR_CHANGE_DELAY,
    VALIDATOR_SET_KIND, credit_grant_payload, validator_set_payload,
};
use darm_core::time::now_ms;
use darm_core::{Hash32, NodeId};
use mesh_llm_plugin::{
    ManifestEntry, OperationRouter, PluginError, json_schema_operation, operation,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::wire::Outbound;

/// Default voting window.
const DEFAULT_DEADLINE_MINUTES: u64 = 24 * 60;
const MAX_DEADLINE_MINUTES: u64 = 30 * 24 * 60;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemberArg {
    /// Node id (hex).
    pub node: String,
    /// Voting weight, at least 1.
    pub weight: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeValidatorsArgs {
    /// The complete new validator set (not a diff).
    pub members: Vec<MemberArg>,
    /// Ledger height the new set applies from. Leave room for the vote and
    /// for every operator to see the change before it takes effect.
    pub effective_height: u64,
    /// Voting window in minutes (default 1440).
    #[serde(default)]
    pub deadline_minutes: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnactValidatorsArgs {
    pub proposal: String,
    /// Exactly the members that were proposed.
    pub members: Vec<MemberArg>,
    /// Exactly the height that was proposed.
    pub effective_height: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeGrantArgs {
    /// Node id (hex) that receives the credits.
    pub to: String,
    pub amount: u64,
    #[serde(default)]
    pub deadline_minutes: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnactGrantArgs {
    pub proposal: String,
    pub to: String,
    pub amount: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VoteArgs {
    pub proposal: String,
    pub support: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposalArgs {
    pub proposal: String,
}

const PROPOSE_VALIDATORS_DESC: &str = "Propose a new validator set, effective at a future ledger height (validators only; needs the governance feature)";
const ENACT_VALIDATORS_DESC: &str =
    "Enact a validator-set proposal that has passed the two-thirds vote";
const PROPOSE_GRANT_DESC: &str =
    "Propose a grant of closed-loop compute credits to a node (needs the credits feature)";
const ENACT_GRANT_DESC: &str = "Issue a credit grant whose proposal has passed";
const VOTE_DESC: &str = "Vote on a ledger proposal (validators only)";
const PROPOSAL_DESC: &str = "Show a proposal's votes and outcome";

pub fn manifest_entries() -> Vec<ManifestEntry> {
    vec![
        operation::<ProposeValidatorsArgs>("darm_gov_propose_validators", PROPOSE_VALIDATORS_DESC)
            .into(),
        operation::<EnactValidatorsArgs>("darm_gov_enact_validators", ENACT_VALIDATORS_DESC).into(),
        operation::<ProposeGrantArgs>("darm_gov_propose_grant", PROPOSE_GRANT_DESC).into(),
        operation::<EnactGrantArgs>("darm_gov_enact_grant", ENACT_GRANT_DESC).into(),
        operation::<VoteArgs>("darm_gov_vote", VOTE_DESC).into(),
        operation::<ProposalArgs>("darm_gov_proposal", PROPOSAL_DESC).into(),
    ]
}

fn bad(error: impl std::fmt::Display) -> PluginError {
    PluginError::invalid_params(error.to_string())
}

fn node_id(hex: &str) -> Result<NodeId, PluginError> {
    NodeId::from_hex(hex).map_err(bad)
}

fn members_from(args: &[MemberArg]) -> Result<Vec<Member>, PluginError> {
    let mut members = args
        .iter()
        .map(|m| {
            Ok(Member {
                node: node_id(&m.node)?,
                weight: m.weight,
            })
        })
        .collect::<Result<Vec<_>, PluginError>>()?;
    members.sort_by_key(|m| m.node);
    Ok(members)
}

fn deadline(minutes: Option<u64>, now: u64) -> Result<u64, PluginError> {
    let minutes = minutes.unwrap_or(DEFAULT_DEADLINE_MINUTES);
    if minutes == 0 || minutes > MAX_DEADLINE_MINUTES {
        return Err(bad("deadline_minutes must be 1..=43200"));
    }
    Ok(now + minutes * 60_000)
}

fn require_validator(node: &DarmNode) -> Result<(), PluginError> {
    if node.ledger().is_validator(&node.node_id()) {
        Ok(())
    } else {
        Err(bad("this node is not in the validator set in force"))
    }
}

fn outcome_name(outcome: ProposalOutcome) -> &'static str {
    match outcome {
        ProposalOutcome::Passed => "passed",
        ProposalOutcome::Rejected => "rejected",
        ProposalOutcome::Pending => "pending",
        ProposalOutcome::Expired => "expired",
    }
}

/// A passed proposal of `kind` whose payload is `payload`, or why not.
fn require_passed(
    node: &DarmNode,
    proposal: &Hash32,
    kind: &str,
    payload: Hash32,
    now: u64,
) -> Result<(), PluginError> {
    let state = node
        .ledger()
        .state()
        .proposal(proposal)
        .ok_or_else(|| bad("no finalized proposal with that id"))?;
    if state.kind != kind {
        return Err(bad(format!(
            "proposal is of kind {:?}, not {kind:?}",
            state.kind
        )));
    }
    if state.payload != payload {
        return Err(bad("arguments differ from what the proposal approved"));
    }
    match state.outcome(node.ledger().validators(), now) {
        ProposalOutcome::Passed => Ok(()),
        other => Err(bad(format!(
            "proposal is {}, not passed",
            outcome_name(other)
        ))),
    }
}

type Step = Result<(Value, Vec<Outbound>), PluginError>;

pub fn propose_validators(node: &mut DarmNode, args: ProposeValidatorsArgs, now: u64) -> Step {
    require_validator(node)?;
    let members = members_from(&args.members)?;
    let earliest = node.ledger().height() + 1 + VALIDATOR_CHANGE_DELAY;
    if args.effective_height < earliest + 2 {
        // The proposal, the vote and the enactment each need a block.
        return Err(bad(format!(
            "effective_height must be at least {} (proposal, vote and enactment each take a block)",
            earliest + 2
        )));
    }
    let body = EntryBody::Proposal {
        kind: VALIDATOR_SET_KIND.into(),
        payload: validator_set_payload(&members, args.effective_height),
        deadline_ms: deadline(args.deadline_minutes, now)?,
    };
    // Shape-check the set now rather than at enactment.
    EntryBody::ValidatorSetChange {
        proposal: Hash32::ZERO,
        members: members.clone(),
        effective_height: args.effective_height,
    }
    .validate_shape(&node.node_id())
    .map_err(bad)?;
    let (id, out) = node.submit_governance(body, now).map_err(bad)?;
    Ok((
        json!({
            "proposal": id,
            "members": members,
            "effective_height": args.effective_height,
            "next": "validators vote with darm_gov_vote; then darm_gov_enact_validators with the same members and height",
        }),
        out,
    ))
}

pub fn enact_validators(node: &mut DarmNode, args: EnactValidatorsArgs, now: u64) -> Step {
    require_validator(node)?;
    let proposal = Hash32::from_hex(&args.proposal).map_err(bad)?;
    let members = members_from(&args.members)?;
    require_passed(
        node,
        &proposal,
        VALIDATOR_SET_KIND,
        validator_set_payload(&members, args.effective_height),
        now,
    )?;
    let body = EntryBody::ValidatorSetChange {
        proposal,
        members,
        effective_height: args.effective_height,
    };
    let (id, out) = node.submit_governance(body, now).map_err(bad)?;
    Ok((
        json!({"ledger_entry": id, "effective_height": args.effective_height}),
        out,
    ))
}

pub fn propose_grant(node: &mut DarmNode, args: ProposeGrantArgs, now: u64) -> Step {
    require_validator(node)?;
    let to = node_id(&args.to)?;
    EntryBody::CreditGrant {
        proposal: Hash32::ZERO,
        to,
        amount: args.amount,
    }
    .validate_shape(&node.node_id())
    .map_err(bad)?;
    let body = EntryBody::Proposal {
        kind: CREDIT_GRANT_KIND.into(),
        payload: credit_grant_payload(&to, args.amount),
        deadline_ms: deadline(args.deadline_minutes, now)?,
    };
    let (id, out) = node.submit_governance(body, now).map_err(bad)?;
    Ok((
        json!({"proposal": id, "to": to, "amount": args.amount,
               "next": "validators vote with darm_gov_vote; then darm_gov_enact_grant"}),
        out,
    ))
}

pub fn enact_grant(node: &mut DarmNode, args: EnactGrantArgs, now: u64) -> Step {
    require_validator(node)?;
    let proposal = Hash32::from_hex(&args.proposal).map_err(bad)?;
    let to = node_id(&args.to)?;
    require_passed(
        node,
        &proposal,
        CREDIT_GRANT_KIND,
        credit_grant_payload(&to, args.amount),
        now,
    )?;
    let body = EntryBody::CreditGrant {
        proposal,
        to,
        amount: args.amount,
    };
    let (id, out) = node.submit_governance(body, now).map_err(bad)?;
    Ok((
        json!({"ledger_entry": id, "to": to, "amount": args.amount}),
        out,
    ))
}

pub fn vote(node: &mut DarmNode, args: VoteArgs, now: u64) -> Step {
    require_validator(node)?;
    let proposal = Hash32::from_hex(&args.proposal).map_err(bad)?;
    if node.ledger().state().proposal(&proposal).is_none() {
        return Err(bad("no finalized proposal with that id"));
    }
    let body = EntryBody::Vote {
        proposal,
        support: args.support,
    };
    let (id, out) = node.submit_governance(body, now).map_err(bad)?;
    Ok((json!({"ledger_entry": id, "support": args.support}), out))
}

pub fn proposal(node: &DarmNode, args: ProposalArgs, now: u64) -> Result<Value, PluginError> {
    let id = Hash32::from_hex(&args.proposal).map_err(bad)?;
    let state = node
        .ledger()
        .state()
        .proposal(&id)
        .ok_or_else(|| bad("no finalized proposal with that id"))?;
    Ok(json!({
        "proposal": id,
        "kind": state.kind,
        "payload": state.payload,
        "author": state.author,
        "deadline_ms": state.deadline_ms,
        "yes": state.yes,
        "no": state.no,
        "outcome": outcome_name(state.outcome(node.ledger().validators(), now)),
    }))
}

macro_rules! register_step {
    ($router:expr, $node:expr, $args:ty, $name:literal, $desc:expr, $f:path) => {{
        let n = Arc::clone($node);
        $router.add_json(
            json_schema_operation::<$args>($name, $desc),
            move |args: $args, context| {
                let n = Arc::clone(&n);
                Box::pin(async move {
                    let (response, outbound) = {
                        let mut node = n.lock().expect("darm node lock");
                        $f(&mut node, args, now_ms())?
                    };
                    send_outbound(context, &n, outbound)
                        .await
                        .map_err(|e| PluginError::internal(e.to_string()))?;
                    Ok(response)
                })
            },
        );
    }};
}

pub fn register(router: &mut OperationRouter, node: &SharedNode) {
    register_step!(
        router,
        node,
        ProposeValidatorsArgs,
        "darm_gov_propose_validators",
        PROPOSE_VALIDATORS_DESC,
        propose_validators
    );
    register_step!(
        router,
        node,
        EnactValidatorsArgs,
        "darm_gov_enact_validators",
        ENACT_VALIDATORS_DESC,
        enact_validators
    );
    register_step!(
        router,
        node,
        ProposeGrantArgs,
        "darm_gov_propose_grant",
        PROPOSE_GRANT_DESC,
        propose_grant
    );
    register_step!(
        router,
        node,
        EnactGrantArgs,
        "darm_gov_enact_grant",
        ENACT_GRANT_DESC,
        enact_grant
    );
    register_step!(router, node, VoteArgs, "darm_gov_vote", VOTE_DESC, vote);
    let n = Arc::clone(node);
    router.add_json(
        json_schema_operation::<ProposalArgs>("darm_gov_proposal", PROPOSAL_DESC),
        move |args: ProposalArgs, _context| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                let node = n.lock().expect("darm node lock");
                proposal(&node, args, now_ms())
            })
        },
    );
}
