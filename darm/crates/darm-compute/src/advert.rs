//! Signed capability advertisements.
//!
//! A node gossips what it offers to the pool right now. Adverts are
//! short-lived (TTL at most ten minutes), carry a per-node sequence number so
//! an old advert cannot be replayed over a newer one, are bound to one DARM
//! network (chain id), and are signed over a canonical encoding like every
//! other DARM record. What a node *claims* is only a prior: the pool corrects
//! throughput from measured receipts, so inflating numbers does not buy work.

use crate::device::{Conditions, FormFactor, MAX_DEVICES, OfferedDevice, WorkloadKind};
use darm_core::canonical::Canonical;
use darm_core::{DarmError, Hash32, NodeId, NodeIdentity, Result, Signature};
use serde::{Deserialize, Serialize};

pub const MAX_TTL_MS: u64 = 10 * 60 * 1_000;
pub const MIN_TTL_MS: u64 = 5_000;
pub const MAX_CLOCK_SKEW_MS: u64 = 2 * 60 * 1_000;
pub const MAX_WORKLOADS: usize = 8;
pub const MAX_PEER_ID_BYTES: usize = 128;
/// Claims above this (2 PFLOPS per device) are rejected outright.
pub const MAX_DEVICE_GFLOPS: u32 = 2_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAdvert {
    pub node: NodeId,
    pub network: Hash32,
    pub seq: u64,
    pub issued_ms: u64,
    pub ttl_ms: u64,
    pub form_factor: FormFactor,
    pub devices: Vec<OfferedDevice>,
    pub workloads: Vec<WorkloadKind>,
    pub conditions: Conditions,
    /// The mesh endpoint this node serves from, so routing can map a mesh
    /// candidate to its compute profile. Only trusted when the advert
    /// arrived from that endpoint (see `ComputePool::upsert`).
    #[serde(default)]
    pub mesh_peer: Option<String>,
    /// The mesh host's own signature binding this DARM node id to
    /// `mesh_peer` (mesh-llm `PluginKey`). When present and valid, the
    /// endpoint mapping is proven rather than claimed.
    #[serde(default)]
    pub mesh_binding: Option<MeshBinding>,
}

/// mesh-llm plugin-key binding: the mesh endpoint (an Ed25519 key) signs
/// `"mesh-llm-plugin-key-v1:" || u64le-len-prefixed(endpoint, plugin,
/// public_key, "ed25519")`, where `public_key` is the DARM node id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshBinding {
    pub plugin: String,
    pub signature: Signature,
}

const MESH_BINDING_DOMAIN: &[u8] = b"mesh-llm-plugin-key-v1:";

fn write_le_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// The bytes a mesh endpoint signs to bind `node` for `plugin`.
pub fn mesh_binding_input(endpoint: &[u8; 32], plugin: &str, node: &NodeId) -> Vec<u8> {
    let mut out = MESH_BINDING_DOMAIN.to_vec();
    write_le_prefixed(&mut out, endpoint);
    write_le_prefixed(&mut out, plugin.as_bytes());
    write_le_prefixed(&mut out, node.as_bytes());
    write_le_prefixed(&mut out, b"ed25519");
    out
}

impl MeshBinding {
    /// Verify that mesh endpoint `endpoint_hex` bound `node`.
    pub fn verify(&self, endpoint_hex: &str, node: &NodeId) -> Result<()> {
        if self.plugin.is_empty() || self.plugin.len() > MAX_PEER_ID_BYTES {
            return Err(DarmError::malformed("mesh binding", "bad plugin name"));
        }
        let endpoint = NodeId::from_hex(endpoint_hex)?;
        endpoint.verify(
            &mesh_binding_input(endpoint.as_bytes(), &self.plugin, node),
            &self.signature,
        )
    }
}

fn valid_peer_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PEER_ID_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl CapabilityAdvert {
    pub fn canonical(&self) -> Vec<u8> {
        let mut encoder = Canonical::new("compute-advert");
        encoder
            .fixed32(self.node.as_bytes())
            .fixed32(self.network.as_bytes())
            .u64(self.seq)
            .u64(self.issued_ms)
            .u64(self.ttl_ms)
            .u8(self.form_factor.code());
        encoder.u32(self.devices.len() as u32);
        for offered in &self.devices {
            let d = &offered.device;
            encoder
                .u8(d.kind.code())
                .u8(d.backend.code())
                .str(&d.label)
                .u32(d.memory_mb)
                .u32(d.compute_units)
                .u32(d.gflops)
                .bool(d.unified_memory)
                .u8(offered.share_pct);
        }
        encoder.u32(self.workloads.len() as u32);
        for workload in &self.workloads {
            encoder.u8(workload.code());
        }
        let c = &self.conditions;
        encoder
            .bool(c.on_battery)
            .bool(c.charging)
            .u8(c.battery_pct)
            .bool(c.metered)
            .u8(c.thermal.code())
            .u16(c.load_milli);
        match &self.mesh_peer {
            Some(peer) => encoder.u8(1).str(peer),
            None => encoder.u8(0),
        };
        match &self.mesh_binding {
            Some(binding) => {
                let Signature::Ed25519(bytes) = &binding.signature;
                encoder.u8(1).str(&binding.plugin).bytes(bytes)
            }
            None => encoder.u8(0),
        };
        encoder.finish()
    }

    /// Stateless checks (everything except signature and clock).
    pub fn validate(&self) -> Result<()> {
        if self.devices.len() > MAX_DEVICES {
            return Err(DarmError::LimitExceeded("advert devices".into()));
        }
        if self.workloads.is_empty() || self.workloads.len() > MAX_WORKLOADS {
            return Err(DarmError::malformed("advert", "1..=8 workloads"));
        }
        for (i, workload) in self.workloads.iter().enumerate() {
            if self.workloads[..i].contains(workload) {
                return Err(DarmError::malformed("advert", "duplicate workload"));
            }
        }
        for offered in &self.devices {
            offered.device.validate()?;
            if !(1..=100).contains(&offered.share_pct) {
                return Err(DarmError::malformed("advert", "share_pct out of range"));
            }
            if offered.device.gflops > MAX_DEVICE_GFLOPS {
                return Err(DarmError::malformed("advert", "implausible throughput"));
            }
        }
        if !(MIN_TTL_MS..=MAX_TTL_MS).contains(&self.ttl_ms) {
            return Err(DarmError::malformed("advert", "ttl out of range"));
        }
        if self.conditions.battery_pct > 100 || self.conditions.load_milli > 1_000 {
            return Err(DarmError::malformed("advert", "conditions out of range"));
        }
        if let Some(peer) = &self.mesh_peer
            && !valid_peer_id(peer)
        {
            return Err(DarmError::malformed("advert", "bad mesh peer id"));
        }
        if self.mesh_binding.is_some() && self.mesh_peer.is_none() {
            return Err(DarmError::malformed(
                "advert",
                "binding without a mesh peer",
            ));
        }
        Ok(())
    }

    /// Whether the mesh endpoint mapping is proven by the mesh host's key.
    pub fn mesh_binding_valid(&self) -> bool {
        match (&self.mesh_peer, &self.mesh_binding) {
            (Some(peer), Some(binding)) => binding.verify(peer, &self.node).is_ok(),
            _ => false,
        }
    }

    pub fn expires_ms(&self) -> u64 {
        self.issued_ms.saturating_add(self.ttl_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedAdvert {
    pub advert: CapabilityAdvert,
    pub signature: Signature,
}

impl SignedAdvert {
    pub fn sign(identity: &NodeIdentity, advert: CapabilityAdvert) -> Result<Self> {
        if advert.node != identity.node_id() {
            return Err(DarmError::Unauthorized("advert for another node".into()));
        }
        advert.validate()?;
        let signature = identity.sign(&advert.canonical());
        Ok(Self { advert, signature })
    }

    pub fn id(&self) -> Hash32 {
        Hash32::digest("compute-advert-id", &self.advert.canonical())
    }

    pub fn verify(&self, network: &Hash32, now_ms: u64) -> Result<()> {
        let advert = &self.advert;
        if &advert.network != network {
            return Err(DarmError::Rejected("advert for another network".into()));
        }
        advert.validate()?;
        if advert.issued_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            return Err(DarmError::Rejected("advert issued in the future".into()));
        }
        if advert.expires_ms() <= now_ms {
            return Err(DarmError::Rejected("advert expired".into()));
        }
        advert.node.verify(&advert.canonical(), &self.signature)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::device::{ComputeProfile, ContributionPolicy, Device};

    pub const NOW: u64 = 1_700_000_000_000;

    pub fn network() -> Hash32 {
        Hash32::digest("net", b"test")
    }

    pub fn advert(
        identity: &NodeIdentity,
        seq: u64,
        gpus: Vec<Device>,
        form: FormFactor,
    ) -> SignedAdvert {
        let profile = ComputeProfile {
            form_factor: form,
            devices: std::iter::once(Device::cpu(8, 16_384, form.is_mobile()))
                .chain(gpus)
                .collect(),
            workloads: vec![WorkloadKind::Embedding, WorkloadKind::Inference],
        };
        let conditions = Conditions::default();
        let advert = CapabilityAdvert {
            node: identity.node_id(),
            network: network(),
            seq,
            issued_ms: NOW,
            ttl_ms: 60_000,
            form_factor: form,
            devices: profile.offer(
                &ContributionPolicy::for_form_factor(FormFactor::Server),
                &conditions,
            ),
            workloads: profile.workloads.clone(),
            conditions,
            mesh_peer: Some(format!("peer{}", identity.node_id().short())),
            mesh_binding: None,
        };
        SignedAdvert::sign(identity, advert).unwrap()
    }

    #[test]
    fn verifies_and_rejects_tampering() {
        let id = NodeIdentity::from_secret_bytes(&[1; 32]);
        let signed = advert(&id, 1, vec![], FormFactor::Server);
        signed.verify(&network(), NOW).unwrap();
        let mut tampered = signed.clone();
        tampered.advert.devices[0].device.gflops *= 10;
        assert_eq!(
            tampered.verify(&network(), NOW),
            Err(DarmError::InvalidSignature)
        );
        assert!(signed.verify(&Hash32::ZERO, NOW).is_err(), "other network");
        assert!(signed.verify(&network(), NOW + 60_000).is_err(), "expired");
        assert!(
            signed
                .verify(&network(), NOW - MAX_CLOCK_SKEW_MS - 1)
                .is_err(),
            "from the future"
        );
    }

    #[test]
    fn rejects_bad_shapes() {
        let id = NodeIdentity::from_secret_bytes(&[1; 32]);
        let base = advert(&id, 1, vec![], FormFactor::Server).advert;
        let mut long_ttl = base.clone();
        long_ttl.ttl_ms = MAX_TTL_MS + 1;
        assert!(SignedAdvert::sign(&id, long_ttl).is_err());
        let mut no_work = base.clone();
        no_work.workloads.clear();
        assert!(SignedAdvert::sign(&id, no_work).is_err());
        let mut bad_peer = base.clone();
        bad_peer.mesh_peer = Some("peer/../../etc".into());
        assert!(SignedAdvert::sign(&id, bad_peer).is_err());
        let mut huge = base.clone();
        huge.devices[0].device.gflops = MAX_DEVICE_GFLOPS + 1;
        assert!(SignedAdvert::sign(&id, huge).is_err());
        let other = NodeIdentity::from_secret_bytes(&[2; 32]);
        assert!(
            SignedAdvert::sign(&other, base).is_err(),
            "cannot sign for others"
        );
    }

    #[test]
    fn mesh_binding_proves_the_endpoint() {
        let darm = NodeIdentity::from_secret_bytes(&[1; 32]);
        // The mesh endpoint key is a different Ed25519 key held by the host.
        let endpoint = NodeIdentity::from_secret_bytes(&[50; 32]);
        let endpoint_hex = endpoint.node_id().to_hex();
        let signature = endpoint.sign(&mesh_binding_input(
            endpoint.node_id().as_bytes(),
            "darm",
            &darm.node_id(),
        ));
        let mut advert = advert(&darm, 1, vec![], FormFactor::Server).advert;
        advert.mesh_peer = Some(endpoint_hex);
        advert.mesh_binding = Some(MeshBinding {
            plugin: "darm".into(),
            signature,
        });
        assert!(advert.mesh_binding_valid());
        // Replaying that binding for another endpoint or node fails.
        let mut moved = advert.clone();
        moved.mesh_peer = Some(
            NodeIdentity::from_secret_bytes(&[51; 32])
                .node_id()
                .to_hex(),
        );
        assert!(!moved.mesh_binding_valid());
        let mut stolen = advert.clone();
        stolen.node = NodeIdentity::from_secret_bytes(&[2; 32]).node_id();
        assert!(!stolen.mesh_binding_valid());
        let mut unbound = advert;
        unbound.mesh_peer = None;
        assert!(unbound.validate().is_err());
    }
}
