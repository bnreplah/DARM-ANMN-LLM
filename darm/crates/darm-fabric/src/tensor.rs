//! Tensor and activation streams.
//!
//! Distributed inference moves numbers, not text: hidden states between
//! pipeline stages, embeddings, logits. Carrying them as JSON costs several
//! bytes per value and a parse on both ends. Here a tensor is its raw
//! little-endian elements behind a small binary header, split into chunks
//! that each fit one sealed frame, so a `tight` chunk from another stream can
//! always slip in between two chunks of a large tensor.
//!
//! Chunk payload (integers big-endian, elements little-endian):
//!
//! ```text
//! u32 index | u32 count | u32 total_len | u8 dtype | u8 ndim | ndim x u32 dim
//!   | u32 scale (f32 bits) | 32-byte digest | data
//! ```
//!
//! Every chunk repeats the metadata, so chunks may arrive in any order and a
//! receiver can reject a stream on its first chunk. All chunks except the
//! last carry exactly [`chunk_data_len`] bytes, which makes `count` a
//! function of `total_len` and leaves a sender no freedom to craft odd
//! layouts. The digest covers the metadata and the whole tensor and is
//! checked once every chunk is in.
//!
//! `I8` tensors are symmetric linear quantizations of `f32` data
//! (`value = scale * i8`): a quarter of the bytes for activations that
//! tolerate it. Quantization is lossy; the error bound is `scale / 2` per
//! element.

use crate::channel::{ChannelId, MAX_SINGLE_FRAME_PAYLOAD};
use crate::link::Link;
use darm_core::{DarmError, Hash32, PROTOCOL, Result};
use std::collections::HashMap;

pub const MAX_DIMS: usize = 8;
const FIXED_HEADER: usize = 4 + 4 + 4 + 1 + 1 + 4 + 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DType {
    F32 = 1,
    I8 = 2,
}

impl DType {
    pub fn size(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::I8 => 1,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::F32),
            2 => Ok(Self::I8),
            _ => Err(DarmError::malformed("tensor", "unknown dtype")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    pub dtype: DType,
    pub shape: Vec<u32>,
    /// Quantization step for `I8`; `1.0` for `F32`.
    pub scale: f32,
    /// Elements in row-major order, little-endian.
    pub data: Vec<u8>,
}

/// Bytes of tensor data per chunk for a tensor of `ndim` dimensions.
pub fn chunk_data_len(ndim: usize) -> usize {
    MAX_SINGLE_FRAME_PAYLOAD - FIXED_HEADER - 4 * ndim
}

fn expected_len(dtype: DType, shape: &[u32]) -> Result<usize> {
    if shape.is_empty() || shape.len() > MAX_DIMS {
        return Err(DarmError::malformed("tensor", "1 to 8 dimensions required"));
    }
    shape
        .iter()
        .try_fold(dtype.size(), |acc, &dim| {
            if dim == 0 {
                return None;
            }
            acc.checked_mul(usize::try_from(dim).ok()?)
        })
        .ok_or_else(|| DarmError::malformed("tensor", "zero or overflowing shape"))
}

fn digest(meta: &[u8], data: &[u8]) -> Hash32 {
    let mut hasher = blake3::Hasher::new_derive_key(&format!("{PROTOCOL} fabric-tensor"));
    hasher.update(meta);
    hasher.update(data);
    Hash32(*hasher.finalize().as_bytes())
}

impl Tensor {
    pub fn from_f32(shape: &[u32], values: &[f32]) -> Result<Self> {
        let tensor = Self {
            dtype: DType::F32,
            shape: shape.to_vec(),
            scale: 1.0,
            data: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        };
        tensor.validate()?;
        Ok(tensor)
    }

    /// Quantize to 8 bits with a symmetric scale chosen from the largest
    /// magnitude. Non-finite input is refused rather than silently clamped.
    pub fn quantize_i8(shape: &[u32], values: &[f32]) -> Result<Self> {
        if values.iter().any(|v| !v.is_finite()) {
            return Err(DarmError::malformed("tensor", "non-finite value"));
        }
        let max = values.iter().fold(0f32, |acc, v| acc.max(v.abs()));
        let scale = if max == 0.0 { 1.0 } else { max / 127.0 };
        let data = values
            .iter()
            .map(|v| (v / scale).round().clamp(-127.0, 127.0) as i8 as u8)
            .collect();
        let tensor = Self {
            dtype: DType::I8,
            shape: shape.to_vec(),
            scale,
            data,
        };
        tensor.validate()?;
        Ok(tensor)
    }

    /// The values as `f32`, dequantizing if needed.
    pub fn to_f32(&self) -> Vec<f32> {
        match self.dtype {
            DType::F32 => {
                let (words, _) = self.data.as_chunks::<4>();
                words.iter().map(|b| f32::from_le_bytes(*b)).collect()
            }
            DType::I8 => self
                .data
                .iter()
                .map(|&b| f32::from(b as i8) * self.scale)
                .collect(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if expected_len(self.dtype, &self.shape)? != self.data.len() {
            return Err(DarmError::malformed("tensor", "data does not match shape"));
        }
        if u32::try_from(self.data.len()).is_err() {
            return Err(DarmError::LimitExceeded("tensor".into()));
        }
        if !self.scale.is_finite() || self.scale <= 0.0 {
            return Err(DarmError::malformed("tensor", "bad scale"));
        }
        Ok(())
    }

    /// Metadata bytes shared by every chunk (everything after `count`,
    /// before the digest).
    fn meta(&self) -> Vec<u8> {
        let mut meta = Vec::with_capacity(10 + 4 * self.shape.len());
        meta.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        meta.push(self.dtype as u8);
        meta.push(self.shape.len() as u8);
        for dim in &self.shape {
            meta.extend_from_slice(&dim.to_be_bytes());
        }
        meta.extend_from_slice(&self.scale.to_bits().to_be_bytes());
        meta
    }

    /// Chunk payloads, each sized for one sealed frame.
    pub fn chunks(&self) -> Result<Vec<Vec<u8>>> {
        self.validate()?;
        let meta = self.meta();
        let digest = digest(&meta, &self.data);
        let step = chunk_data_len(self.shape.len());
        let count = self.data.len().div_ceil(step) as u32;
        Ok(self
            .data
            .chunks(step)
            .enumerate()
            .map(|(index, data)| {
                let mut out = Vec::with_capacity(FIXED_HEADER + meta.len() + data.len());
                out.extend_from_slice(&(index as u32).to_be_bytes());
                out.extend_from_slice(&count.to_be_bytes());
                out.extend_from_slice(&meta);
                out.extend_from_slice(digest.as_bytes());
                out.extend_from_slice(data);
                out
            })
            .collect())
    }
}

/// Queue a whole tensor on `channel` as stream `stream`. Nothing is queued
/// unless every chunk fits the channel's outbound budget, so a receiver is
/// never left with a tensor that the sender could not finish.
pub fn send_tensor(
    link: &mut Link,
    channel: ChannelId,
    stream: u32,
    tensor: &Tensor,
    now_ms: u64,
) -> Result<()> {
    let chunks = tensor.chunks()?;
    let total: usize = chunks
        .iter()
        .map(|c| c.len() + crate::envelope::HEADER_LEN)
        .sum();
    if total > link.free_outbound_bytes(channel)? {
        return Err(DarmError::LimitExceeded(
            "tensor does not fit the outbound queue".into(),
        ));
    }
    let last = chunks.len() - 1;
    for (index, chunk) in chunks.into_iter().enumerate() {
        link.send(channel, stream, index == last, chunk, now_ms)?;
    }
    Ok(())
}

struct ChunkHeader<'a> {
    index: usize,
    count: usize,
    total_len: usize,
    dtype: DType,
    shape: Vec<u32>,
    scale: f32,
    /// `total_len .. scale` as sent, compared across chunks of one stream.
    meta: &'a [u8],
    digest: [u8; 32],
    data: &'a [u8],
}

fn be_u32(bytes: &[u8], at: usize) -> usize {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
}

fn parse_chunk(payload: &[u8]) -> Result<ChunkHeader<'_>> {
    let short = || DarmError::malformed("tensor chunk", "truncated header");
    if payload.len() < FIXED_HEADER + 4 {
        return Err(short());
    }
    let dtype = DType::from_code(payload[12])?;
    let ndim = usize::from(payload[13]);
    if ndim == 0 || ndim > MAX_DIMS {
        return Err(DarmError::malformed("tensor chunk", "bad dimension count"));
    }
    let meta_end = 14 + 4 * ndim + 4;
    if payload.len() < meta_end + 32 {
        return Err(short());
    }
    let shape = (0..ndim)
        .map(|i| be_u32(payload, 14 + 4 * i) as u32)
        .collect();
    let scale = f32::from_bits(be_u32(payload, meta_end - 4) as u32);
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&payload[meta_end..meta_end + 32]);
    Ok(ChunkHeader {
        index: be_u32(payload, 0),
        count: be_u32(payload, 4),
        total_len: be_u32(payload, 8),
        dtype,
        shape,
        scale,
        meta: &payload[8..meta_end],
        digest,
        data: &payload[meta_end + 32..],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReassemblyLimits {
    /// Largest tensor accepted, in bytes.
    pub max_tensor_bytes: usize,
    /// Tensors that may be partially received at once.
    pub max_streams: usize,
    /// Bytes buffered across all partial tensors.
    pub max_pending_bytes: usize,
    /// A partial tensor is discarded after this long.
    pub timeout_ms: u64,
}

impl Default for ReassemblyLimits {
    fn default() -> Self {
        Self {
            max_tensor_bytes: 64 << 20,
            max_streams: 8,
            max_pending_bytes: 128 << 20,
            timeout_ms: 5_000,
        }
    }
}

struct Partial {
    meta: Vec<u8>,
    digest: [u8; 32],
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
    started_ms: u64,
}

/// Reassembles tensors from one peer's chunks. Use one per link.
///
/// Memory is committed only for bytes that actually arrived: a chunk that
/// announces a huge tensor reserves a small slot table, not the tensor.
pub struct Reassembler {
    limits: ReassemblyLimits,
    partial: HashMap<u32, Partial>,
    pending_bytes: usize,
}

impl Reassembler {
    pub fn new(limits: ReassemblyLimits) -> Self {
        Self {
            limits,
            partial: HashMap::new(),
            pending_bytes: 0,
        }
    }

    pub fn pending_streams(&self) -> usize {
        self.partial.len()
    }

    fn drop_stream(&mut self, stream: u32) {
        if let Some(partial) = self.partial.remove(&stream) {
            self.pending_bytes -= partial.bytes;
        }
    }

    fn expire(&mut self, now_ms: u64) {
        let timeout = self.limits.timeout_ms;
        let stale: Vec<u32> = self
            .partial
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.started_ms) >= timeout)
            .map(|(&stream, _)| stream)
            .collect();
        for stream in stale {
            self.drop_stream(stream);
        }
    }

    /// Check a chunk against the layout its own metadata implies.
    fn check_layout(&self, header: &ChunkHeader<'_>) -> Result<()> {
        if header.total_len > self.limits.max_tensor_bytes {
            return Err(DarmError::LimitExceeded("tensor".into()));
        }
        if expected_len(header.dtype, &header.shape)? != header.total_len {
            return Err(DarmError::malformed(
                "tensor chunk",
                "length does not match shape",
            ));
        }
        if !header.scale.is_finite() || header.scale <= 0.0 {
            return Err(DarmError::malformed("tensor chunk", "bad scale"));
        }
        let step = chunk_data_len(header.shape.len());
        if header.count != header.total_len.div_ceil(step) || header.index >= header.count {
            return Err(DarmError::malformed(
                "tensor chunk",
                "bad chunk index or count",
            ));
        }
        let expected = if header.index + 1 == header.count {
            header.total_len - step * (header.count - 1)
        } else {
            step
        };
        if header.data.len() != expected {
            return Err(DarmError::malformed("tensor chunk", "bad chunk length"));
        }
        Ok(())
    }

    /// Feed one chunk. Returns the tensor once its last chunk arrives and
    /// the digest verifies. A stream that fails any check is discarded.
    pub fn on_chunk(&mut self, stream: u32, payload: &[u8], now_ms: u64) -> Result<Option<Tensor>> {
        self.expire(now_ms);
        let header = parse_chunk(payload)?;
        self.check_layout(&header)?;
        if !self.partial.contains_key(&stream) {
            if self.partial.len() >= self.limits.max_streams {
                return Err(DarmError::LimitExceeded("partial tensors".into()));
            }
            self.partial.insert(
                stream,
                Partial {
                    meta: header.meta.to_vec(),
                    digest: header.digest,
                    chunks: vec![None; header.count],
                    received: 0,
                    bytes: 0,
                    started_ms: now_ms,
                },
            );
        }
        if let Err(error) = self.store(stream, &header) {
            if error != DarmError::Duplicate {
                self.drop_stream(stream);
            }
            return Err(error);
        }
        let complete = self
            .partial
            .get(&stream)
            .is_some_and(|p| p.received == p.chunks.len());
        if !complete {
            return Ok(None);
        }
        self.finish(stream, &header).map(Some)
    }

    fn store(&mut self, stream: u32, header: &ChunkHeader<'_>) -> Result<()> {
        if self.pending_bytes + header.data.len() > self.limits.max_pending_bytes {
            return Err(DarmError::LimitExceeded("pending tensor bytes".into()));
        }
        let partial = self.partial.get_mut(&stream).expect("inserted above");
        if partial.meta != header.meta || partial.digest != header.digest {
            return Err(DarmError::malformed(
                "tensor chunk",
                "metadata changed mid-stream",
            ));
        }
        if partial.chunks[header.index].is_some() {
            return Err(DarmError::Duplicate);
        }
        partial.chunks[header.index] = Some(header.data.to_vec());
        partial.received += 1;
        partial.bytes += header.data.len();
        self.pending_bytes += header.data.len();
        Ok(())
    }

    fn finish(&mut self, stream: u32, header: &ChunkHeader<'_>) -> Result<Tensor> {
        let partial = self.partial.remove(&stream).expect("complete stream");
        self.pending_bytes -= partial.bytes;
        let data: Vec<u8> = partial.chunks.into_iter().flatten().flatten().collect();
        if digest(&partial.meta, &data).as_bytes() != &partial.digest {
            return Err(DarmError::Rejected("tensor digest mismatch".into()));
        }
        Ok(Tensor {
            dtype: header.dtype,
            shape: header.shape.clone(),
            scale: header.scale,
            data,
        })
    }
}

#[cfg(test)]
mod tests;
