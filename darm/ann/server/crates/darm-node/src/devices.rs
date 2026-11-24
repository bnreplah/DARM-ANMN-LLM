//! Device companions: phones that hold their own identity, enroll with the
//! archive, sign every request, and log verification work.
//!
//! A device's id is its Ed25519 public key, which is exactly a DARM-ANN
//! [`NodeId`], so everything the device does can be checked against the
//! ledger with nothing but the record. Access is never a bearer secret:
//!
//! - **Enrollment** is a signature over [`enroll_message`] made with the
//!   device key (proof of possession, bound to this chain).
//! - **Requests** that act for a device carry a signature over
//!   [`request_message`], which binds the method, target, body hash, a
//!   timestamp and a random nonce, so a captured request cannot be replayed
//!   or redirected.
//! - **Work receipts** are signatures over [`work_message`]; they stay in the
//!   append-only device log as the auditable record of what the device did.
//!
//! The archive does not pay anyone. Credits move only through ledger entries
//! (spec §4.7); this module records, verifies and reports, nothing more.

use darm_core::canonical::Canonical;
use darm_core::{DarmError, Hash32, NodeId, Result, Signature};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::PathBuf;

pub mod routes;
pub mod wallet;

#[cfg(test)]
mod tests;

pub const MAX_DEVICES: usize = 10_000;
/// Largest clock difference tolerated between a device and the archive.
pub const CLOCK_SKEW_MS: u64 = 120_000;
pub const MAX_NONCES: usize = 100_000;
pub const MAX_RECEIPTS_PER_DEVICE: usize = 1_000;
pub const MAX_LABEL_BYTES: usize = 32;
/// Widest block range one verification receipt may claim.
pub const MAX_RECEIPT_BLOCKS: u64 = 10_000;
pub const NONCE_BYTES: usize = 16;
/// The one work kind this archive understands: re-verifying a block range.
pub const KIND_LEDGER_VERIFY: &str = "ledger-verify";
const MAX_LOG_LINE_BYTES: usize = 8 * 1024;

pub const PLATFORMS: [&str; 5] = ["ios", "android", "web", "desktop", "other"];

/// A device's signed enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub node: NodeId,
    pub platform: String,
    pub label: String,
    /// When the device made the enrollment (its clock).
    pub created_ms: u64,
    /// When the archive accepted it (the archive's clock).
    pub enrolled_ms: u64,
    pub signature: Signature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every block in the range verified.
    Ok,
    /// The device found a block that does not verify.
    Invalid,
}

impl Verdict {
    fn code(self) -> u8 {
        match self {
            Self::Ok => 1,
            Self::Invalid => 2,
        }
    }
}

/// What a device claims to have verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkClaim {
    pub kind: String,
    pub from_height: u64,
    pub to_height: u64,
    /// Hash of the block at `to_height` as the device saw it.
    pub head: Hash32,
    pub verdict: Verdict,
    pub elapsed_ms: u32,
    pub completed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedWork {
    pub node: NodeId,
    #[serde(flatten)]
    pub claim: WorkClaim,
    pub signature: Signature,
}

pub fn enroll_message(
    chain: &Hash32,
    node: &NodeId,
    platform: &str,
    label: &str,
    created_ms: u64,
) -> Vec<u8> {
    let mut encoder = Canonical::new("device-enroll");
    encoder
        .fixed32(chain.as_bytes())
        .fixed32(node.as_bytes())
        .str(platform)
        .str(label)
        .u64(created_ms);
    encoder.finish()
}

/// Hash of a request body, as bound into [`request_message`].
pub fn body_hash(body: &[u8]) -> Hash32 {
    Hash32::digest("device-body", body)
}

pub fn request_message(
    chain: &Hash32,
    node: &NodeId,
    method: &str,
    target: &str,
    timestamp_ms: u64,
    nonce: &[u8; NONCE_BYTES],
    body: &[u8],
) -> Vec<u8> {
    let mut encoder = Canonical::new("device-request");
    encoder
        .fixed32(chain.as_bytes())
        .fixed32(node.as_bytes())
        .str(method)
        .str(target)
        .u64(timestamp_ms)
        .bytes(nonce)
        .fixed32(body_hash(body).as_bytes());
    encoder.finish()
}

pub fn work_message(chain: &Hash32, node: &NodeId, claim: &WorkClaim) -> Vec<u8> {
    let mut encoder = Canonical::new("device-work");
    encoder
        .fixed32(chain.as_bytes())
        .fixed32(node.as_bytes())
        .str(&claim.kind)
        .u64(claim.from_height)
        .u64(claim.to_height)
        .fixed32(claim.head.as_bytes())
        .u8(claim.verdict.code())
        .u32(claim.elapsed_ms)
        .u64(claim.completed_ms);
    encoder.finish()
}

fn check_text(what: &'static str, value: &str, allow_space: bool) -> Result<()> {
    if value.len() > MAX_LABEL_BYTES {
        return Err(DarmError::malformed(what, "too long"));
    }
    let ok = value.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') || (allow_space && b == b' ')
    });
    if ok {
        Ok(())
    } else {
        Err(DarmError::malformed(
            what,
            "allowed characters: letters, digits, '_', '-', '.' (and spaces in labels)",
        ))
    }
}

/// A device log line.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LogLine {
    Enroll(DeviceRecord),
    Work(SignedWork),
}

/// Enrolled devices, their signed work, and the replay cache.
pub struct Devices {
    chain: Hash32,
    records: BTreeMap<NodeId, DeviceRecord>,
    work: BTreeMap<NodeId, Vec<SignedWork>>,
    nonces: HashMap<(NodeId, [u8; NONCE_BYTES]), u64>,
    log: Option<PathBuf>,
}

impl Devices {
    pub fn new(chain: Hash32) -> Self {
        Self {
            chain,
            records: BTreeMap::new(),
            work: BTreeMap::new(),
            nonces: HashMap::new(),
            log: None,
        }
    }

    /// Open the append-only device log under `dir`, re-verifying every line.
    pub fn open(chain: Hash32, dir: &std::path::Path) -> Result<Self> {
        let path = dir.join(format!("devices-{}.jsonl", chain.short()));
        let mut devices = Self::new(chain);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                for (number, line) in text.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if line.len() > MAX_LOG_LINE_BYTES {
                        return Err(DarmError::LimitExceeded("device log line".into()));
                    }
                    let parsed: LogLine = serde_json::from_str(line).map_err(|e| {
                        DarmError::malformed("device log", format!("line {}: {e}", number + 1))
                    })?;
                    match parsed {
                        LogLine::Enroll(record) => devices.insert_record(record)?,
                        LogLine::Work(work) => devices.insert_work(work)?,
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        devices.log = Some(path);
        Ok(devices)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn get(&self, node: &NodeId) -> Option<&DeviceRecord> {
        self.records.get(node)
    }

    pub fn work(&self, node: &NodeId) -> &[SignedWork] {
        self.work.get(node).map_or(&[], Vec::as_slice)
    }

    fn append(&self, line: &LogLine) -> Result<()> {
        let Some(path) = &self.log else {
            return Ok(());
        };
        let mut text = serde_json::to_string(line)
            .map_err(|e| DarmError::malformed("device log", e.to_string()))?;
        text.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(text.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    fn insert_record(&mut self, record: DeviceRecord) -> Result<()> {
        check_text("platform", &record.platform, false)?;
        if !PLATFORMS.contains(&record.platform.as_str()) {
            return Err(DarmError::malformed("platform", "unknown platform"));
        }
        check_text("label", &record.label, true)?;
        let message = enroll_message(
            &self.chain,
            &record.node,
            &record.platform,
            &record.label,
            record.created_ms,
        );
        record.node.verify(&message, &record.signature)?;
        self.records.insert(record.node, record);
        Ok(())
    }

    fn insert_work(&mut self, work: SignedWork) -> Result<()> {
        if !self.records.contains_key(&work.node) {
            return Err(DarmError::Unauthorized("device is not enrolled".into()));
        }
        let message = work_message(&self.chain, &work.node, &work.claim);
        work.node.verify(&message, &work.signature)?;
        self.work.entry(work.node).or_default().push(work);
        Ok(())
    }

    /// Enroll a device. Returns the record and whether it is new. Enrolling
    /// the same key again with the same signed claim is idempotent; a
    /// different claim for an enrolled key is refused, so a key's identity
    /// cannot be rewritten.
    pub fn enroll(
        &mut self,
        node: NodeId,
        platform: String,
        label: String,
        created_ms: u64,
        signature: Signature,
        now_ms: u64,
    ) -> Result<(DeviceRecord, bool)> {
        if created_ms.abs_diff(now_ms) > CLOCK_SKEW_MS {
            return Err(DarmError::Rejected(
                "enrollment time is too far from the archive's clock".into(),
            ));
        }
        if let Some(existing) = self.records.get(&node) {
            let same = existing.platform == platform
                && existing.label == label
                && existing.created_ms == created_ms
                && existing.signature == signature;
            return if same {
                Ok((existing.clone(), false))
            } else {
                Err(DarmError::Duplicate)
            };
        }
        if self.records.len() >= MAX_DEVICES {
            return Err(DarmError::LimitExceeded("device registry is full".into()));
        }
        let record = DeviceRecord {
            node,
            platform,
            label,
            created_ms,
            enrolled_ms: now_ms,
            signature,
        };
        // Verifies the signature before anything is stored or logged.
        let mut staged = record.clone();
        staged.enrolled_ms = now_ms;
        self.insert_record(staged)?;
        if let Err(error) = self.append(&LogLine::Enroll(record.clone())) {
            self.records.remove(&node);
            return Err(error);
        }
        Ok((record, true))
    }

    /// Record a verified work claim. `known_head` is the hash of the block
    /// the archive holds at `claim.to_height`, if any: an `ok` claim must
    /// agree with it.
    pub fn record_work(
        &mut self,
        work: SignedWork,
        known_head: Option<Hash32>,
    ) -> Result<(SignedWork, bool)> {
        let claim = &work.claim;
        if claim.kind != KIND_LEDGER_VERIFY {
            return Err(DarmError::malformed("work", "unsupported work kind"));
        }
        if claim.from_height == 0
            || claim.from_height > claim.to_height
            || claim.to_height - claim.from_height >= MAX_RECEIPT_BLOCKS
        {
            return Err(DarmError::malformed("work", "block range out of bounds"));
        }
        if claim.verdict == Verdict::Ok && known_head != Some(claim.head) {
            return Err(DarmError::Rejected(
                "the claimed head is not the archive's block at that height".into(),
            ));
        }
        if self.work(&work.node).contains(&work) {
            return Ok((work, false));
        }
        if self.work(&work.node).len() >= MAX_RECEIPTS_PER_DEVICE {
            return Err(DarmError::LimitExceeded(
                "this device has reached its receipt limit".into(),
            ));
        }
        self.insert_work(work.clone())?;
        if let Err(error) = self.append(&LogLine::Work(work.clone())) {
            if let Some(list) = self.work.get_mut(&work.node) {
                list.pop();
            }
            return Err(error);
        }
        Ok((work, true))
    }

    /// Authenticate a request that acts for a device. Checks, in order:
    /// the device is enrolled, the timestamp is fresh, the signature verifies
    /// over this exact request, and only then is the nonce spent, so an
    /// unauthenticated caller cannot fill the replay cache.
    pub fn authenticate(
        &mut self,
        headers: &RequestAuth,
        method: &str,
        target: &str,
        body: &[u8],
        now_ms: u64,
    ) -> Result<NodeId> {
        let node = headers.node;
        if !self.records.contains_key(&node) {
            return Err(DarmError::Unauthorized("device is not enrolled".into()));
        }
        if headers.timestamp_ms.abs_diff(now_ms) > CLOCK_SKEW_MS {
            return Err(DarmError::Unauthorized(
                "request timestamp is outside the allowed window".into(),
            ));
        }
        let message = request_message(
            &self.chain,
            &node,
            method,
            target,
            headers.timestamp_ms,
            &headers.nonce,
            body,
        );
        node.verify(&message, &headers.signature)?;
        if self.nonces.len() >= MAX_NONCES / 2 {
            self.nonces.retain(|_, expires| *expires > now_ms);
        }
        if self.nonces.len() >= MAX_NONCES {
            return Err(DarmError::LimitExceeded("too many recent requests".into()));
        }
        let key = (node, headers.nonce);
        if self.nonces.contains_key(&key) {
            return Err(DarmError::Unauthorized("nonce was already used".into()));
        }
        self.nonces
            .insert(key, now_ms.saturating_add(2 * CLOCK_SKEW_MS));
        Ok(node)
    }
}

/// The four `X-DARM-*` headers of a signed request, parsed.
#[derive(Debug, Clone)]
pub struct RequestAuth {
    pub node: NodeId,
    pub timestamp_ms: u64,
    pub nonce: [u8; NONCE_BYTES],
    pub signature: Signature,
}

impl RequestAuth {
    pub fn parse(headers: &axum::http::HeaderMap) -> Result<Self> {
        let get = |name: &str| -> Result<&str> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| DarmError::Unauthorized(format!("missing header {name}")))
        };
        let node = NodeId::from_hex(get("x-darm-node")?)?;
        let timestamp_ms = get("x-darm-timestamp")?
            .parse()
            .map_err(|_| DarmError::malformed("x-darm-timestamp", "not a number"))?;
        let nonce_hex = get("x-darm-nonce")?;
        let mut nonce = [0u8; NONCE_BYTES];
        if nonce_hex.len() != NONCE_BYTES * 2 || hex_decode(nonce_hex, &mut nonce).is_none() {
            return Err(DarmError::malformed(
                "x-darm-nonce",
                "expected 32 hex digits",
            ));
        }
        let signature = Signature::from_tagged_str(get("x-darm-signature")?)?;
        Ok(Self {
            node,
            timestamp_ms,
            nonce,
            signature,
        })
    }
}

fn hex_decode(text: &str, out: &mut [u8]) -> Option<()> {
    let digits = text.as_bytes();
    if digits.len() != out.len() * 2 {
        return None;
    }
    for (index, slot) in out.iter_mut().enumerate() {
        let high = char::from(*digits.get(index * 2)?).to_digit(16)?;
        let low = char::from(*digits.get(index * 2 + 1)?).to_digit(16)?;
        *slot = u8::try_from(high * 16 + low).ok()?;
    }
    Some(())
}
