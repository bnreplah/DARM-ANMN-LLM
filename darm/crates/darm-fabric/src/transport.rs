//! Carriers: anything that moves opaque frames between two nodes.
//!
//! The fabric never assumes a particular network. A carrier only has to
//! deliver whole frames; it may reorder or drop them (the session tolerates
//! both), and it is never trusted, since every frame is sealed.
//!
//! Two carriers ship here: [`MemoryTransport`] for tests and simulators, and
//! [`TcpTransport`] for real sockets. A mesh-llm plugin channel, a QUIC
//! stream or a serial radio implements the same trait.

use darm_core::{DarmError, Result};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

/// Largest frame a carrier must move: a full session frame (8-byte masked
/// nonce plus a maximal Noise message). Handshake messages are smaller.
pub const MAX_WIRE_FRAME: usize = 8 + 65_535;

pub trait Transport {
    /// Send one frame. Frames above [`MAX_WIRE_FRAME`] are refused.
    fn send(&mut self, frame: &[u8]) -> Result<()>;

    /// Receive one frame, waiting up to `timeout`. `Ok(None)` on timeout.
    /// A zero timeout polls without waiting.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>>;
}

/// In-process carrier backed by bounded queues.
#[derive(Debug)]
pub struct MemoryTransport {
    tx: SyncSender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
}

impl MemoryTransport {
    /// A connected pair. `capacity` bounds the frames in flight each way.
    pub fn pair(capacity: usize) -> (Self, Self) {
        let (a_tx, b_rx) = sync_channel(capacity);
        let (b_tx, a_rx) = sync_channel(capacity);
        (Self { tx: a_tx, rx: a_rx }, Self { tx: b_tx, rx: b_rx })
    }
}

impl Transport for MemoryTransport {
    fn send(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() > MAX_WIRE_FRAME {
            return Err(DarmError::LimitExceeded("wire frame".into()));
        }
        self.tx
            .try_send(frame.to_vec())
            .map_err(|error| match error {
                TrySendError::Full(_) => DarmError::LimitExceeded("carrier queue is full".into()),
                TrySendError::Disconnected(_) => DarmError::Io("carrier closed".into()),
            })
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(DarmError::Io("carrier closed".into())),
        }
    }
}

/// TCP carrier. Frames are `u32 BE length || bytes`.
///
/// Nagle's algorithm is disabled: the fabric already batches by priority,
/// and a 40 ms coalescing delay would dominate tight-class latency.
#[derive(Debug)]
pub struct TcpTransport {
    stream: TcpStream,
    /// Bytes read but not yet returned as a frame. A read timeout can land
    /// mid-frame, so partial input is kept between calls.
    pending: Vec<u8>,
}

impl TcpTransport {
    pub fn new(stream: TcpStream) -> Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            pending: Vec::new(),
        })
    }

    /// A complete frame from buffered input, if one is there. The length
    /// prefix is validated before anything is allocated for the body.
    fn take_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let Some((prefix, body)) = self.pending.split_first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(*prefix) as usize;
        if len > MAX_WIRE_FRAME {
            return Err(DarmError::LimitExceeded("wire frame".into()));
        }
        if body.len() < len {
            return Ok(None);
        }
        let frame = body[..len].to_vec();
        self.pending.drain(..4 + len);
        Ok(Some(frame))
    }
}

impl Transport for TcpTransport {
    fn send(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() > MAX_WIRE_FRAME {
            return Err(DarmError::LimitExceeded("wire frame".into()));
        }
        let len = u32::try_from(frame.len())
            .map_err(|_| DarmError::LimitExceeded("wire frame".into()))?;
        // One write per frame so the length and body leave in one segment.
        let mut wire = Vec::with_capacity(4 + frame.len());
        wire.extend_from_slice(&len.to_be_bytes());
        wire.extend_from_slice(frame);
        self.stream.write_all(&wire)?;
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        if let Some(frame) = self.take_frame()? {
            return Ok(Some(frame));
        }
        // A zero timeout is a poll. The socket API reads `Some(0)` as an
        // error and rounds tiny timeouts up to a scheduler tick, so polling
        // uses non-blocking mode instead.
        let poll = timeout.is_zero();
        if poll {
            self.stream.set_nonblocking(true)?;
        } else {
            self.stream.set_read_timeout(Some(timeout))?;
        }
        let result = self.read_frame();
        if poll {
            self.stream.set_nonblocking(false)?;
        }
        result
    }
}

impl TcpTransport {
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(DarmError::Io("carrier closed".into())),
                Ok(read) => {
                    self.pending.extend_from_slice(&chunk[..read]);
                    if let Some(frame) = self.take_frame()? {
                        return Ok(Some(frame));
                    }
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(None);
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn memory_pair_round_trips_and_bounds_queue() {
        let (mut a, mut b) = MemoryTransport::pair(1);
        a.send(b"one").unwrap();
        assert!(matches!(a.send(b"two"), Err(DarmError::LimitExceeded(_))));
        assert_eq!(b.recv(Duration::from_millis(10)).unwrap().unwrap(), b"one");
        assert!(b.recv(Duration::from_millis(1)).unwrap().is_none());
        assert!(a.send(&vec![0; MAX_WIRE_FRAME + 1]).is_err());
        drop(a);
        assert!(b.recv(Duration::from_millis(1)).is_err());
    }

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn tcp_frames_survive_partial_reads() {
        let (mut raw, server) = tcp_pair();
        let mut transport = TcpTransport::new(server).unwrap();
        // Two frames; the second arrives in two pieces around a timeout.
        raw.write_all(&[0, 0, 0, 2, b'h', b'i', 0, 0, 0, 3, b'a'])
            .unwrap();
        assert_eq!(
            transport.recv(Duration::from_millis(200)).unwrap().unwrap(),
            b"hi"
        );
        assert!(transport.recv(Duration::from_millis(20)).unwrap().is_none());
        assert!(transport.recv(Duration::ZERO).unwrap().is_none(), "poll");
        raw.write_all(b"bc").unwrap();
        assert_eq!(
            transport.recv(Duration::from_millis(200)).unwrap().unwrap(),
            b"abc"
        );
    }

    #[test]
    fn tcp_rejects_oversized_length_prefix_without_allocating() {
        let (mut raw, server) = tcp_pair();
        let mut transport = TcpTransport::new(server).unwrap();
        raw.write_all(&u32::MAX.to_be_bytes()).unwrap();
        assert!(matches!(
            transport.recv(Duration::from_millis(200)),
            Err(DarmError::LimitExceeded(_))
        ));
    }
}
