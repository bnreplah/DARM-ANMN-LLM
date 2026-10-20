//! Adaptive shard sizing.
//!
//! Heterogeneous pools fail on fixed shard sizes: a GPU finishes a shard in
//! milliseconds and idles on coordination overhead, while a phone takes
//! minutes and holds up the job. [`ShardSizer`] keeps an EWMA of each
//! (node, device kind)'s measured rate and sizes the next shard so it lands
//! near a target duration, starting small for unmeasured workers and moving
//! at most 2x per step so one outlier measurement cannot swing it wildly.

use crate::device::DeviceKind;
use darm_core::NodeId;
use std::collections::HashMap;

const MAX_TRACKED: usize = 8_192;

#[derive(Debug, Clone)]
pub struct ShardSizer {
    pub target_ms: u32,
    pub min_units: u32,
    pub max_units: u32,
    rates: HashMap<(NodeId, DeviceKind), (f64, u32)>,
}

impl Default for ShardSizer {
    fn default() -> Self {
        Self::new(2_000, 1, 1 << 20)
    }
}

impl ShardSizer {
    pub fn new(target_ms: u32, min_units: u32, max_units: u32) -> Self {
        Self {
            target_ms: target_ms.max(1),
            min_units: min_units.max(1),
            max_units: max_units.max(min_units.max(1)),
            rates: HashMap::new(),
        }
    }

    /// Units for this worker's next shard.
    pub fn next_units(&self, worker: &NodeId, kind: DeviceKind) -> u32 {
        let Some((rate, _)) = self.rates.get(&(*worker, kind)) else {
            // Probe small: a tenth of the range above the minimum, at most 64.
            return self
                .min_units
                .max(64.min(self.max_units / 10))
                .min(self.max_units);
        };
        let units = rate * f64::from(self.target_ms);
        (units as u32).clamp(self.min_units, self.max_units)
    }

    pub fn observe(&mut self, worker: &NodeId, kind: DeviceKind, units: u32, elapsed_ms: u32) {
        if units == 0 {
            return;
        }
        if self.rates.len() >= MAX_TRACKED && !self.rates.contains_key(&(*worker, kind)) {
            return;
        }
        let sample = f64::from(units) / f64::from(elapsed_ms.max(1));
        let entry = self.rates.entry((*worker, kind)).or_insert((sample, 0));
        if entry.1 > 0 {
            // Bound each step to [0.5x, 2x] of the current estimate.
            let bounded = sample.clamp(entry.0 / 2.0, entry.0 * 2.0);
            entry.0 = 0.7 * entry.0 + 0.3 * bounded;
        }
        entry.1 = entry.1.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_per_device_and_bounds_steps() {
        let mut sizer = ShardSizer::new(1_000, 1, 1_000_000);
        let gpu = NodeId([1; 32]);
        let phone = NodeId([2; 32]);
        let first = sizer.next_units(&gpu, DeviceKind::Gpu);
        assert_eq!(first, 64);
        for _ in 0..30 {
            sizer.observe(&gpu, DeviceKind::Gpu, 10_000, 100); // 100 units/ms
            sizer.observe(&phone, DeviceKind::Cpu, 50, 1_000); // 0.05 units/ms
        }
        let gpu_units = sizer.next_units(&gpu, DeviceKind::Gpu);
        let phone_units = sizer.next_units(&phone, DeviceKind::Cpu);
        assert!((90_000..=110_000).contains(&gpu_units), "{gpu_units}");
        assert!((40..=60).contains(&phone_units), "{phone_units}");
        // One absurd sample moves the estimate by at most a bounded step.
        sizer.observe(&phone, DeviceKind::Cpu, 1_000_000, 1);
        let after = sizer.next_units(&phone, DeviceKind::Cpu);
        assert!(after <= phone_units * 2, "{after} vs {phone_units}");
    }
}
