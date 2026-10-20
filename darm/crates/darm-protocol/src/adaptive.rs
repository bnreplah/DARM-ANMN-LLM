//! Adaptive protocol tuning.
//!
//! One [`AdaptiveController`] per peer link turns measurements into protocol
//! parameters:
//!
//! - **Round-trip time** with the RFC 6298 estimator (SRTT/RTTVAR, RTO with
//!   back-off) from request/response pairs and handshakes;
//! - **Batch size** by AIMD: +1 item per delivered batch, halved on a
//!   timeout, bounded so a single message stays small on slow links;
//! - **Cadence and fan-out** from local device state: a phone on battery or
//!   a metered link gossips less often to fewer peers, stops unsolicited
//!   announcements (pull only), and prefers compact encodings; a hot device
//!   backs off further;
//! - the **local offer** used for polymorphic negotiation, ordering suites
//!   by whether this CPU has AES instructions.

use crate::suite::{Codec, Offer, Padding, Suite};
use serde::{Deserialize, Serialize};

const MIN_RTO_MS: f64 = 200.0;
const MAX_RTO_MS: f64 = 60_000.0;
const INITIAL_RTO_MS: f64 = 3_000.0;
const MIN_BATCH: f64 = 2.0;
const MAX_BATCH: f64 = 64.0;

/// Device state that should change how the protocol behaves. Filled in by
/// the host (or a mobile app through the plugin's operations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHints {
    /// Phone/tablet-class device.
    pub mobile: bool,
    /// Running on battery right now (not on mains, not charging).
    pub on_battery: bool,
    /// 0..=100; ignored unless `on_battery`.
    pub battery_pct: u8,
    /// Data is metered (cellular, hotspot).
    pub metered: bool,
    /// Device reports thermal pressure (serious or worse).
    pub hot: bool,
}

impl Default for DeviceHints {
    fn default() -> Self {
        Self {
            mobile: false,
            on_battery: false,
            battery_pct: 100,
            metered: false,
            hot: false,
        }
    }
}

impl DeviceHints {
    /// How strongly to slow down background protocol work: 1 = normal.
    pub fn slowdown(&self) -> u64 {
        let mut factor = 1;
        if self.on_battery {
            factor *= if self.battery_pct < 20 { 8 } else { 2 };
        }
        if self.metered {
            factor *= 2;
        }
        if self.hot {
            factor *= 2;
        }
        factor.min(32)
    }

    pub fn constrained(&self) -> bool {
        self.mobile || self.on_battery || self.metered
    }
}

/// Whether this CPU accelerates AES (AES-GCM is then the faster suite).
pub fn hardware_aes() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("aes")
            && std::arch::is_x86_feature_detected!("pclmulqdq")
    }
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("aes")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

/// The offer this node makes, shaped by its hardware and device state.
pub fn local_offer(hints: &DeviceHints, aes: bool) -> Offer {
    let suites = if aes {
        vec![Suite::NoiseXxAesGcm, Suite::NoiseXxChaChaPoly]
    } else {
        vec![Suite::NoiseXxChaChaPoly, Suite::NoiseXxAesGcm]
    };
    let codecs = if hints.constrained() {
        vec![Codec::Cbor, Codec::Json]
    } else {
        vec![Codec::Json, Codec::Cbor]
    };
    let paddings = if hints.metered {
        vec![Padding::Minimal, Padding::Block256, Padding::PowerOfTwo]
    } else {
        vec![Padding::PowerOfTwo, Padding::Block256, Padding::Minimal]
    };
    Offer {
        suites,
        codecs,
        paddings,
    }
}

/// RFC 6298 round-trip estimator.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LinkEstimator {
    pub srtt_ms: Option<f64>,
    pub rttvar_ms: f64,
    pub backoff: u32,
    pub samples: u64,
    pub timeouts: u64,
    /// Exponentially weighted loss rate in `[0, 1]`.
    pub loss: f64,
}

impl LinkEstimator {
    pub fn on_rtt(&mut self, rtt_ms: f64) {
        if !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return;
        }
        let rtt_ms = rtt_ms.min(MAX_RTO_MS);
        match self.srtt_ms {
            None => {
                self.srtt_ms = Some(rtt_ms);
                self.rttvar_ms = rtt_ms / 2.0;
            }
            Some(srtt) => {
                self.rttvar_ms = 0.75 * self.rttvar_ms + 0.25 * (srtt - rtt_ms).abs();
                self.srtt_ms = Some(0.875 * srtt + 0.125 * rtt_ms);
            }
        }
        self.backoff = 0;
        self.samples += 1;
        self.loss *= 0.9;
    }

    pub fn on_timeout(&mut self) {
        self.timeouts += 1;
        self.backoff = (self.backoff + 1).min(6);
        self.loss = 0.9 * self.loss + 0.1;
    }

    /// Retransmission/response timeout in milliseconds.
    pub fn rto_ms(&self) -> u64 {
        let base = match self.srtt_ms {
            Some(srtt) => srtt + (4.0 * self.rttvar_ms).max(10.0),
            None => INITIAL_RTO_MS,
        };
        let backed_off = base * f64::from(1u32 << self.backoff);
        backed_off.clamp(MIN_RTO_MS, MAX_RTO_MS) as u64
    }
}

/// Protocol parameters for one link at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Tuning {
    /// Items (memories, blocks, ids) per message.
    pub batch_items: usize,
    /// Multiplier on periodic gossip intervals.
    pub interval_factor: u64,
    /// How many peers receive unsolicited pushes.
    pub fanout: usize,
    /// Whether to send unsolicited announcements at all.
    pub announce: bool,
    pub request_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdaptiveController {
    pub link: LinkEstimator,
    batch: f64,
}

impl Default for AdaptiveController {
    fn default() -> Self {
        Self {
            link: LinkEstimator::default(),
            batch: 16.0,
        }
    }
}

impl AdaptiveController {
    /// A request on this link was answered after `rtt_ms`.
    pub fn on_delivery(&mut self, rtt_ms: f64) {
        self.link.on_rtt(rtt_ms);
        self.batch = (self.batch + 1.0).min(MAX_BATCH);
    }

    /// A request on this link was not answered within the timeout.
    pub fn on_timeout(&mut self) {
        self.link.on_timeout();
        self.batch = (self.batch / 2.0).max(MIN_BATCH);
    }

    pub fn tuning(&self, hints: &DeviceHints) -> Tuning {
        let slowdown = hints.slowdown();
        let mut batch = self.batch;
        if hints.metered {
            batch = batch.min(8.0);
        }
        // High measured loss: smaller messages lose less work per drop.
        if self.link.loss > 0.3 {
            batch = (batch / 2.0).max(MIN_BATCH);
        }
        let fanout = if hints.constrained() {
            2
        } else if self.link.loss > 0.3 {
            4
        } else {
            8
        };
        Tuning {
            batch_items: batch as usize,
            interval_factor: slowdown,
            fanout,
            announce: !(hints.metered || (hints.on_battery && hints.battery_pct < 20)),
            request_timeout_ms: self.link.rto_ms(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6298_estimator_and_backoff() {
        let mut link = LinkEstimator::default();
        assert_eq!(link.rto_ms(), 3_000);
        link.on_rtt(100.0);
        assert_eq!(link.srtt_ms, Some(100.0));
        assert_eq!(link.rto_ms(), 300);
        link.on_timeout();
        assert_eq!(link.rto_ms(), 600);
        for _ in 0..20 {
            link.on_timeout();
        }
        assert!(link.rto_ms() <= 60_000);
        link.on_rtt(f64::NAN);
        link.on_rtt(-5.0);
        assert_eq!(link.samples, 1, "garbage samples are ignored");
    }

    #[test]
    fn aimd_batching() {
        let mut controller = AdaptiveController::default();
        let hints = DeviceHints::default();
        let start = controller.tuning(&hints).batch_items;
        controller.on_delivery(50.0);
        let grown = start + 1;
        assert_eq!(controller.tuning(&hints).batch_items, grown);
        controller.on_timeout();
        assert_eq!(controller.tuning(&hints).batch_items, grown / 2);
        for _ in 0..20 {
            controller.on_timeout();
        }
        assert_eq!(controller.tuning(&hints).batch_items, MIN_BATCH as usize);
        for _ in 0..200 {
            controller.on_delivery(50.0);
        }
        assert_eq!(controller.tuning(&hints).batch_items, MAX_BATCH as usize);
    }

    #[test]
    fn constrained_devices_back_off() {
        let controller = AdaptiveController::default();
        let server = controller.tuning(&DeviceHints::default());
        let phone = controller.tuning(&DeviceHints {
            mobile: true,
            on_battery: true,
            battery_pct: 15,
            metered: true,
            hot: false,
        });
        assert!(phone.interval_factor > server.interval_factor);
        assert!(phone.fanout < server.fanout);
        assert!(phone.batch_items <= 8);
        assert!(!phone.announce, "pull-only on metered/low battery");
        assert!(server.announce);
    }

    #[test]
    fn offer_is_polymorphic() {
        let phone = local_offer(
            &DeviceHints {
                mobile: true,
                metered: true,
                ..DeviceHints::default()
            },
            false,
        );
        assert_eq!(phone.suites[0], Suite::NoiseXxChaChaPoly);
        assert_eq!(phone.codecs[0], Codec::Cbor);
        assert_eq!(phone.paddings[0], Padding::Minimal);
        let server = local_offer(&DeviceHints::default(), true);
        assert_eq!(server.suites[0], Suite::NoiseXxAesGcm);
        assert_eq!(server.codecs[0], Codec::Json);
        server.validate().unwrap();
        phone.validate().unwrap();
    }
}
