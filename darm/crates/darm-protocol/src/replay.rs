//! Sliding anti-replay window over explicit 64-bit nonces (RFC 6479 style).
//!
//! Frames may arrive out of order over gossip, so the receiver accepts any
//! nonce newer than the highest seen, or within the last [`WINDOW`] nonces if
//! not seen before. Callers must only [`ReplayWindow::commit`] after the frame
//! authenticated, so forged frames cannot advance or poison the window.

pub const WINDOW: u64 = 1_024;
const WORDS: usize = (WINDOW / 64) as usize;

#[derive(Debug, Clone, Default)]
pub struct ReplayWindow {
    /// Highest committed nonce + 1 (0 = nothing seen yet).
    top: u64,
    bits: [u64; WORDS],
}

impl ReplayWindow {
    fn slot(nonce: u64) -> (usize, u64) {
        let bit = nonce % WINDOW;
        ((bit / 64) as usize, 1u64 << (bit % 64))
    }

    /// Whether `nonce` would be accepted.
    pub fn check(&self, nonce: u64) -> bool {
        if nonce >= self.top {
            return true;
        }
        if self.top - nonce > WINDOW {
            return false;
        }
        let (word, mask) = Self::slot(nonce);
        self.bits[word] & mask == 0
    }

    /// Record an authenticated `nonce`. Returns false if it was a replay.
    pub fn commit(&mut self, nonce: u64) -> bool {
        if !self.check(nonce) {
            return false;
        }
        if nonce >= self.top {
            // Clear the slots that slide out of the window.
            let advance = nonce + 1 - self.top;
            if advance >= WINDOW {
                self.bits = [0; WORDS];
            } else {
                for n in self.top..=nonce {
                    let (word, mask) = Self::slot(n);
                    self.bits[word] &= !mask;
                }
            }
            self.top = nonce + 1;
        }
        let (word, mask) = Self::slot(nonce);
        self.bits[word] |= mask;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_replays_and_stale_nonces() {
        let mut window = ReplayWindow::default();
        assert!(window.commit(0));
        assert!(!window.commit(0));
        assert!(window.commit(5));
        assert!(window.commit(3), "out of order inside the window");
        assert!(!window.commit(3));
        assert!(window.commit(5_000));
        assert!(!window.check(5_000 - WINDOW - 1), "too old");
        assert!(window.check(5_000 - 10));
        assert!(window.commit(5_000 - 10));
        assert!(!window.commit(5_000 - 10));
    }

    #[test]
    fn sliding_clears_reused_slots() {
        let mut window = ReplayWindow::default();
        assert!(window.commit(1));
        // Nonce 1 + WINDOW maps to the same bit; it must still be accepted.
        assert!(window.commit(1 + WINDOW));
        assert!(!window.commit(1 + WINDOW));
        assert!(!window.check(1), "1 is now outside the window");
    }
}
