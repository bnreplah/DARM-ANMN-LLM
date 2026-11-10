//! Mounting workloads on a link.
//!
//! A workload is anything that speaks on a channel: memory gossip, the
//! ledger, a compute executor, a tensor pipeline, or something that does not
//! exist yet. The fabric does not know what a workload does; it only
//! guarantees who the message came from and that the channel's limits held.
//!
//! [`Endpoint`] joins a carrier and a [`Link`]; [`Dispatcher`] routes what
//! the endpoint receives to the workload mounted on that channel.

use crate::channel::{ChannelId, ChannelTable};
use crate::establish;
use crate::link::{Inbound, Link};
use crate::scheduler::QueueLimits;
use crate::transport::Transport;
use darm_core::time::now_ms;
use darm_core::{DarmError, Result};
use darm_protocol::HandshakeConfig;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

/// A message a workload wants sent back on the link it was called from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    pub channel: ChannelId,
    pub stream: u32,
    pub fin: bool,
    pub payload: Vec<u8>,
}

pub trait Workload {
    /// Handle one message. `message.peer` is the proven sender. Returned
    /// messages are queued on the same link.
    fn on_message(&mut self, message: &Inbound) -> Result<Vec<Outbound>>;
}

#[derive(Default)]
pub struct Dispatcher {
    handlers: HashMap<ChannelId, Box<dyn Workload + Send>>,
}

impl Dispatcher {
    pub fn mount(&mut self, channel: ChannelId, workload: Box<dyn Workload + Send>) -> Result<()> {
        if self.handlers.contains_key(&channel) {
            return Err(DarmError::Duplicate);
        }
        self.handlers.insert(channel, workload);
        Ok(())
    }

    pub fn dispatch(&mut self, message: &Inbound) -> Result<Vec<Outbound>> {
        match self.handlers.get_mut(&message.channel) {
            Some(workload) => workload.on_message(message),
            None => Err(DarmError::Rejected(format!(
                "no workload on channel {}",
                message.channel.0
            ))),
        }
    }
}

/// A link attached to a carrier.
pub struct Endpoint<T: Transport> {
    transport: T,
    link: Link,
    /// Sealed frames the carrier has not accepted yet. A sealed frame has
    /// consumed its nonce, so it is kept and retried rather than re-sealed.
    unsent: VecDeque<Vec<u8>>,
}

impl<T: Transport> Endpoint<T> {
    /// Handshake as initiator and attach.
    pub fn connect(
        mut transport: T,
        config: &HandshakeConfig<'_>,
        channels: ChannelTable,
        limits: QueueLimits,
        timeout: Duration,
    ) -> Result<Self> {
        let session = establish::connect(&mut transport, config, timeout)?;
        Ok(Self::attach(
            transport,
            Link::new(session, channels, limits),
        ))
    }

    /// Handshake as responder and attach.
    pub fn accept(
        mut transport: T,
        config: &HandshakeConfig<'_>,
        channels: ChannelTable,
        limits: QueueLimits,
        timeout: Duration,
    ) -> Result<Self> {
        let session = establish::accept(&mut transport, config, timeout)?;
        Ok(Self::attach(
            transport,
            Link::new(session, channels, limits),
        ))
    }

    pub fn attach(transport: T, link: Link) -> Self {
        Self {
            transport,
            link,
            unsent: VecDeque::new(),
        }
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    pub fn link_mut(&mut self) -> &mut Link {
        &mut self.link
    }

    /// Queue a message. Call [`Endpoint::flush`] to put it on the carrier.
    pub fn send(
        &mut self,
        channel: ChannelId,
        stream: u32,
        fin: bool,
        payload: Vec<u8>,
    ) -> Result<u32> {
        self.link.send(channel, stream, fin, payload, now_ms())
    }

    /// Hand queued messages to the carrier in priority order. Returns
    /// `false` when the carrier pushed back and work remains.
    pub fn flush(&mut self) -> Result<bool> {
        self.flush_limit(usize::MAX)
    }

    /// Like [`Endpoint::flush`], but stops after `max_frames` frames so an
    /// event loop can bound the work done per turn. Returns `false` when
    /// frames remain.
    pub fn flush_limit(&mut self, max_frames: usize) -> Result<bool> {
        let mut sent = 0;
        loop {
            while let Some(frame) = self.unsent.front() {
                if sent >= max_frames {
                    return Ok(false);
                }
                match self.transport.send(frame) {
                    Ok(()) => {
                        self.unsent.pop_front();
                        sent += 1;
                    }
                    Err(DarmError::LimitExceeded(_)) => return Ok(false),
                    Err(error) => return Err(error),
                }
            }
            if sent >= max_frames {
                return Ok(self.link.scheduler().is_empty());
            }
            match self.link.poll_outbound(now_ms())? {
                Some(frames) => self.unsent.extend(frames),
                None => return Ok(true),
            }
        }
    }

    /// Wait up to `timeout` for one message. Frames that fail a check are
    /// counted in the link's stats and reported as errors; the link stays
    /// usable, and the caller decides whether a peer has earned a disconnect.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>> {
        match self.transport.recv(timeout)? {
            Some(frame) => self.link.on_frame(&frame, now_ms()),
            None => Ok(None),
        }
    }

    /// Receive one message, run its workload and queue the replies.
    /// Returns whether a message was handled.
    pub fn serve_once(&mut self, dispatcher: &mut Dispatcher, timeout: Duration) -> Result<bool> {
        let Some(message) = self.recv(timeout)? else {
            return Ok(false);
        };
        for reply in dispatcher.dispatch(&message)? {
            self.send(reply.channel, reply.stream, reply.fin, reply.payload)?;
        }
        self.flush()?;
        Ok(true)
    }
}
