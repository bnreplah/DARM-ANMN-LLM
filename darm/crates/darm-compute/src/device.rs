//! Device profiles, live conditions and contribution policy.

use darm_core::{DarmError, Result};
use serde::{Deserialize, Serialize};

pub const MAX_DEVICES: usize = 16;
pub const MAX_LABEL_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Cpu,
    Gpu,
    /// Neural/AI accelerator (phone NPUs, edge TPUs).
    Npu,
}

impl DeviceKind {
    pub fn code(self) -> u8 {
        match self {
            Self::Cpu => 1,
            Self::Gpu => 2,
            Self::Npu => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Cpu,
    Cuda,
    Rocm,
    Metal,
    Vulkan,
    Other,
}

impl Backend {
    pub fn code(self) -> u8 {
        match self {
            Self::Cpu => 1,
            Self::Cuda => 2,
            Self::Rocm => 3,
            Self::Metal => 4,
            Self::Vulkan => 5,
            Self::Other => 6,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "cpu" => Self::Cpu,
            "cuda" => Self::Cuda,
            "rocm" | "hip" => Self::Rocm,
            "metal" => Self::Metal,
            "vulkan" => Self::Vulkan,
            "other" => Self::Other,
            _ => return None,
        })
    }

    pub fn kind(self) -> DeviceKind {
        match self {
            Self::Cpu => DeviceKind::Cpu,
            _ => DeviceKind::Gpu,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormFactor {
    Server,
    Desktop,
    Laptop,
    Phone,
    Tablet,
    Edge,
}

impl FormFactor {
    pub fn code(self) -> u8 {
        match self {
            Self::Server => 1,
            Self::Desktop => 2,
            Self::Laptop => 3,
            Self::Phone => 4,
            Self::Tablet => 5,
            Self::Edge => 6,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "server" => Self::Server,
            "desktop" => Self::Desktop,
            "laptop" => Self::Laptop,
            "phone" | "mobile" => Self::Phone,
            "tablet" => Self::Tablet,
            "edge" => Self::Edge,
            _ => return None,
        })
    }

    pub fn is_mobile(self) -> bool {
        matches!(self, Self::Phone | Self::Tablet)
    }

    pub fn battery_powered(self) -> bool {
        matches!(self, Self::Phone | Self::Tablet | Self::Laptop)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thermal {
    Nominal,
    Fair,
    Serious,
    Critical,
}

impl Thermal {
    pub fn code(self) -> u8 {
        match self {
            Self::Nominal => 0,
            Self::Fair => 1,
            Self::Serious => 2,
            Self::Critical => 3,
        }
    }
}

/// Named kinds of work. A node advertises only the kinds its operator
/// enabled; nothing else can be scheduled on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    /// LLM inference served through the mesh.
    Inference,
    /// Embedding batches (memory recall, indexing).
    Embedding,
    /// Approximate-nearest-neighbour index builds over memory shards.
    Indexing,
    /// Gradient/training shards for small DARM networks.
    Training,
    /// Re-executing another worker's shard to check it.
    Verification,
}

impl WorkloadKind {
    pub fn code(self) -> u8 {
        match self {
            Self::Inference => 1,
            Self::Embedding => 2,
            Self::Indexing => 3,
            Self::Training => 4,
            Self::Verification => 5,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "inference" => Self::Inference,
            "embedding" => Self::Embedding,
            "indexing" => Self::Indexing,
            "training" => Self::Training,
            "verification" => Self::Verification,
            _ => return None,
        })
    }
}

/// One processor. Throughput is a relative score (≈ GFLOPS at the precision
/// the node serves) used as a prior; the pool corrects it from measurements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub kind: DeviceKind,
    pub backend: Backend,
    pub label: String,
    pub memory_mb: u32,
    /// CPU cores or GPU compute units.
    pub compute_units: u32,
    pub gflops: u32,
    /// Shares memory with the CPU (Apple silicon, phone SoCs, iGPUs).
    pub unified_memory: bool,
}

impl Device {
    pub fn validate(&self) -> Result<()> {
        if self.label.len() > MAX_LABEL_BYTES || self.label.chars().any(char::is_control) {
            return Err(DarmError::malformed(
                "device",
                "label too long or has controls",
            ));
        }
        if self.backend.kind() != self.kind && self.kind != DeviceKind::Npu {
            return Err(DarmError::malformed(
                "device",
                "backend does not match kind",
            ));
        }
        if self.gflops == 0 || self.compute_units == 0 {
            return Err(DarmError::malformed("device", "zero throughput"));
        }
        Ok(())
    }

    /// Rough prior for a CPU when the operator gave none: ~16 GFLOPS per
    /// core (AVX2/NEON FP32 at sustained clocks), less on mobile cores.
    pub fn cpu(cores: u32, memory_mb: u32, mobile: bool) -> Self {
        let per_core = if mobile { 6 } else { 16 };
        Self {
            kind: DeviceKind::Cpu,
            backend: Backend::Cpu,
            label: "cpu".into(),
            memory_mb,
            compute_units: cores.max(1),
            gflops: cores.max(1).saturating_mul(per_core),
            unified_memory: true,
        }
    }

    /// Parse an operator GPU spec `backend:memory_mb:gflops[:label]`, e.g.
    /// `cuda:24576:82000:rtx4090`.
    pub fn parse_gpu(spec: &str) -> Result<Self> {
        let parts: Vec<&str> = spec.trim().split(':').collect();
        if !(3..=4).contains(&parts.len()) {
            return Err(DarmError::malformed(
                "gpu spec",
                "expected backend:memory_mb:gflops[:label]",
            ));
        }
        let backend = Backend::parse(parts[0])
            .filter(|b| *b != Backend::Cpu)
            .ok_or_else(|| DarmError::malformed("gpu spec", "unknown GPU backend"))?;
        let number = |s: &str| {
            s.trim()
                .parse::<u32>()
                .map_err(|_| DarmError::malformed("gpu spec", "expected integers"))
        };
        let device = Self {
            kind: DeviceKind::Gpu,
            backend,
            label: parts
                .get(3)
                .map_or_else(|| parts[0].to_string(), |s| s.to_string()),
            memory_mb: number(parts[1])?,
            compute_units: 1,
            gflops: number(parts[2])?,
            unified_memory: backend == Backend::Metal,
        };
        device.validate()?;
        Ok(device)
    }
}

/// Live device state. Reported by the host or a mobile app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conditions {
    pub on_battery: bool,
    pub charging: bool,
    pub battery_pct: u8,
    pub metered: bool,
    pub thermal: Thermal,
    /// Current load on this node, 0..=1000.
    pub load_milli: u16,
}

impl Default for Conditions {
    fn default() -> Self {
        Self {
            on_battery: false,
            charging: false,
            battery_pct: 100,
            metered: false,
            thermal: Thermal::Nominal,
            load_milli: 0,
        }
    }
}

/// What the device owner agreed to contribute. Defaults depend on the form
/// factor and are conservative for anything battery powered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionPolicy {
    pub contribute: bool,
    pub allow_cpu: bool,
    pub allow_gpu: bool,
    /// Share of each device offered to the pool, 1..=100.
    pub max_share_pct: u8,
    pub require_charging: bool,
    pub min_battery_pct: u8,
    pub allow_metered: bool,
    pub max_thermal: Thermal,
}

impl ContributionPolicy {
    pub fn for_form_factor(form: FormFactor) -> Self {
        match form {
            FormFactor::Phone | FormFactor::Tablet => Self {
                contribute: true,
                allow_cpu: true,
                allow_gpu: true,
                max_share_pct: 50,
                require_charging: true,
                min_battery_pct: 50,
                allow_metered: false,
                max_thermal: Thermal::Fair,
            },
            FormFactor::Laptop => Self {
                contribute: true,
                allow_cpu: true,
                allow_gpu: true,
                max_share_pct: 75,
                require_charging: true,
                min_battery_pct: 30,
                allow_metered: false,
                max_thermal: Thermal::Fair,
            },
            FormFactor::Server | FormFactor::Desktop | FormFactor::Edge => Self {
                contribute: true,
                allow_cpu: true,
                allow_gpu: true,
                max_share_pct: 100,
                require_charging: false,
                min_battery_pct: 0,
                allow_metered: true,
                max_thermal: Thermal::Serious,
            },
        }
    }

    /// Why the device should not contribute right now, if it should not.
    pub fn blocked_by(&self, conditions: &Conditions) -> Option<&'static str> {
        if !self.contribute {
            return Some("contribution disabled by owner");
        }
        if conditions.thermal > self.max_thermal {
            return Some("device too hot");
        }
        if conditions.metered && !self.allow_metered {
            return Some("metered network");
        }
        if conditions.on_battery {
            if self.require_charging && !conditions.charging {
                return Some("on battery and not charging");
            }
            if conditions.battery_pct < self.min_battery_pct {
                return Some("battery below owner minimum");
            }
        }
        None
    }
}

/// A device as offered to the pool right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedDevice {
    pub device: Device,
    /// Effective share offered, 1..=100.
    pub share_pct: u8,
}

/// Everything this node knows about its own compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeProfile {
    pub form_factor: FormFactor,
    pub devices: Vec<Device>,
    pub workloads: Vec<WorkloadKind>,
}

impl ComputeProfile {
    /// Detect the CPU and add operator-declared GPUs.
    pub fn detect(
        form_factor: FormFactor,
        gpus: Vec<Device>,
        workloads: Vec<WorkloadKind>,
    ) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
            .unwrap_or(1);
        let mut devices = vec![Device::cpu(
            cores,
            system_memory_mb(),
            form_factor.is_mobile(),
        )];
        devices.extend(gpus.into_iter().take(MAX_DEVICES - 1));
        Self {
            form_factor,
            devices,
            workloads,
        }
    }

    /// The devices offered under `policy` and `conditions`. Empty when the
    /// owner's policy says not to contribute now. Load and heat shrink the
    /// share so a busy or warm device is asked for less.
    pub fn offer(
        &self,
        policy: &ContributionPolicy,
        conditions: &Conditions,
    ) -> Vec<OfferedDevice> {
        if policy.blocked_by(conditions).is_some() {
            return Vec::new();
        }
        let mut share = u32::from(policy.max_share_pct.clamp(1, 100));
        share = share * u32::from(1_000 - conditions.load_milli.min(900)) / 1_000;
        if conditions.thermal == Thermal::Fair {
            share = share * 3 / 4;
        } else if conditions.thermal >= Thermal::Serious {
            share /= 2;
        }
        let share = share.clamp(1, 100) as u8;
        self.devices
            .iter()
            .filter(|d| match d.kind {
                DeviceKind::Cpu => policy.allow_cpu,
                DeviceKind::Gpu | DeviceKind::Npu => policy.allow_gpu,
            })
            .map(|device| OfferedDevice {
                device: device.clone(),
                share_pct: share,
            })
            .collect()
    }
}

fn system_memory_mb() -> u32 {
    #[cfg(target_os = "linux")]
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo")
        && let Some(kb) = meminfo
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|rest| {
                rest.trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
    {
        return u32::try_from(kb / 1_024).unwrap_or(u32::MAX);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gpu_specs_strictly() {
        let gpu = Device::parse_gpu("cuda:24576:82000:rtx4090").unwrap();
        assert_eq!(gpu.backend, Backend::Cuda);
        assert_eq!(gpu.memory_mb, 24_576);
        assert_eq!(gpu.label, "rtx4090");
        assert!(
            Device::parse_gpu("metal:16384:5000")
                .unwrap()
                .unified_memory
        );
        assert!(Device::parse_gpu("cpu:1:1").is_err());
        assert!(Device::parse_gpu("cuda:x:1").is_err());
        assert!(Device::parse_gpu("cuda:1:0").is_err());
        assert!(Device::parse_gpu("cuda:1").is_err());
        assert!(Device::parse_gpu(&format!("cuda:1:1:{}", "x".repeat(65))).is_err());
    }

    #[test]
    fn phones_only_contribute_when_the_owner_policy_allows() {
        let profile =
            ComputeProfile::detect(FormFactor::Phone, vec![], vec![WorkloadKind::Embedding]);
        let policy = ContributionPolicy::for_form_factor(FormFactor::Phone);
        let unplugged = Conditions {
            on_battery: true,
            charging: false,
            battery_pct: 90,
            ..Conditions::default()
        };
        assert!(profile.offer(&policy, &unplugged).is_empty());
        let charging = Conditions {
            charging: true,
            ..unplugged
        };
        assert_eq!(profile.offer(&policy, &charging).len(), 1);
        let metered = Conditions {
            metered: true,
            ..charging
        };
        assert!(profile.offer(&policy, &metered).is_empty());
        let hot = Conditions {
            thermal: Thermal::Serious,
            ..charging
        };
        assert!(profile.offer(&policy, &hot).is_empty());
        let low = Conditions {
            battery_pct: 20,
            ..charging
        };
        assert!(profile.offer(&policy, &low).is_empty());
    }

    #[test]
    fn load_and_heat_shrink_the_share() {
        let profile = ComputeProfile::detect(
            FormFactor::Server,
            vec![Device::parse_gpu("rocm:65536:160000").unwrap()],
            vec![WorkloadKind::Inference],
        );
        let policy = ContributionPolicy::for_form_factor(FormFactor::Server);
        let idle = profile.offer(&policy, &Conditions::default());
        assert_eq!(idle.len(), 2, "CPU and GPU are both pooled");
        assert_eq!(idle[0].share_pct, 100);
        let busy = profile.offer(
            &policy,
            &Conditions {
                load_milli: 500,
                thermal: Thermal::Fair,
                ..Conditions::default()
            },
        );
        assert_eq!(busy[0].share_pct, 37);
        let cpu_only = ContributionPolicy {
            allow_gpu: false,
            ..policy
        };
        let offered = profile.offer(&cpu_only, &Conditions::default());
        assert!(offered.iter().all(|d| d.device.kind == DeviceKind::Cpu));
    }
}
