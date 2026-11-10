//! Running the session handshake over a carrier.
//!
//! The handshake itself (Noise XX, negotiated suite, identity binding) is
//! `darm-protocol`'s. This module only moves its messages: each is one JSON
//! frame, bounded before parsing, and the whole exchange has a deadline so a
//! silent peer cannot hold a slot open.

use crate::transport::Transport;
use darm_core::time::now_ms;
use darm_core::{DarmError, Result};
use darm_protocol::{HandshakeConfig, HandshakeMsg, Initiator, ResponderStep, Session, respond};
use std::time::{Duration, Instant};

/// Largest handshake frame accepted. The Noise payload is capped at 8 KiB by
/// `darm-protocol`; base64 and the offer lists fit well inside this.
pub const MAX_HANDSHAKE_FRAME: usize = 16 * 1024;

fn send_msg<T: Transport>(transport: &mut T, message: &HandshakeMsg) -> Result<()> {
    let bytes = serde_json::to_vec(message)
        .map_err(|e| DarmError::malformed("handshake", e.to_string()))?;
    transport.send(&bytes)
}

fn recv_msg<T: Transport>(transport: &mut T, deadline: Instant) -> Result<HandshakeMsg> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(DarmError::Rejected("handshake timed out".into()));
    }
    let Some(frame) = transport.recv(remaining)? else {
        return Err(DarmError::Rejected("handshake timed out".into()));
    };
    if frame.len() > MAX_HANDSHAKE_FRAME {
        return Err(DarmError::LimitExceeded("handshake frame".into()));
    }
    serde_json::from_slice(&frame).map_err(|e| DarmError::malformed("handshake", e.to_string()))
}

/// Open a session as the initiator.
pub fn connect<T: Transport>(
    transport: &mut T,
    config: &HandshakeConfig<'_>,
    timeout: Duration,
) -> Result<Session> {
    let deadline = Instant::now() + timeout;
    let (mut initiator, init) = Initiator::start(config)?;
    send_msg(transport, &init)?;
    // At most one retry is honoured; `on_retry` refuses a second.
    for _ in 0..2 {
        let reply = recv_msg(transport, deadline)?;
        if reply.sid() != initiator.sid_hex() {
            return Err(DarmError::Rejected("handshake for another session".into()));
        }
        match reply {
            HandshakeMsg::Retry { offer, .. } => {
                let (next, init) = initiator.on_retry(config, &offer)?;
                initiator = next;
                send_msg(transport, &init)?;
            }
            HandshakeMsg::Resp { noise, .. } => {
                let (session, fin) = initiator.on_resp(config, &noise, now_ms())?;
                send_msg(transport, &fin)?;
                return Ok(session);
            }
            HandshakeMsg::Init { .. } | HandshakeMsg::Fin { .. } => {
                return Err(DarmError::Rejected("unexpected handshake step".into()));
            }
        }
    }
    Err(DarmError::Rejected("handshake did not converge".into()))
}

/// Accept a session as the responder.
pub fn accept<T: Transport>(
    transport: &mut T,
    config: &HandshakeConfig<'_>,
    timeout: Duration,
) -> Result<Session> {
    let deadline = Instant::now() + timeout;
    // The first `Init` may name a suite negotiation does not select; the
    // initiator then gets one retry.
    for _ in 0..2 {
        let HandshakeMsg::Init {
            sid,
            offer,
            suite,
            noise,
        } = recv_msg(transport, deadline)?
        else {
            return Err(DarmError::Rejected("expected handshake init".into()));
        };
        match respond(config, &sid, &offer, suite, &noise)? {
            ResponderStep::Retry(retry) => send_msg(transport, &retry)?,
            ResponderStep::Continue(pending, resp) => {
                send_msg(transport, &resp)?;
                let HandshakeMsg::Fin {
                    sid: fin_sid,
                    noise,
                } = recv_msg(transport, deadline)?
                else {
                    return Err(DarmError::Rejected("expected handshake fin".into()));
                };
                if fin_sid != sid {
                    return Err(DarmError::Rejected("handshake for another session".into()));
                }
                return pending.on_fin(config, &noise, now_ms());
            }
        }
    }
    Err(DarmError::Rejected("handshake did not converge".into()))
}
