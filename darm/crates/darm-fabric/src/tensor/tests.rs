use super::*;

const NOW: u64 = 1_000;

fn ramp(len: usize) -> Vec<f32> {
    (0..len).map(|i| (i as f32 * 0.37).sin()).collect()
}

fn reassemble(chunks: &[Vec<u8>]) -> Result<Option<Tensor>> {
    let mut reassembler = Reassembler::new(ReassemblyLimits::default());
    let mut out = None;
    for chunk in chunks {
        out = reassembler.on_chunk(7, chunk, NOW)?;
    }
    Ok(out)
}

#[test]
fn single_chunk_round_trip() {
    let values = ramp(4096);
    let tensor = Tensor::from_f32(&[1, 4096], &values).unwrap();
    let chunks = tensor.chunks().unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].len() <= MAX_SINGLE_FRAME_PAYLOAD);
    let back = reassemble(&chunks).unwrap().unwrap();
    assert_eq!(back, tensor);
    assert_eq!(back.to_f32(), values);
}

#[test]
fn large_tensor_reassembles_from_shuffled_chunks() {
    let values = ramp(200_000);
    let tensor = Tensor::from_f32(&[200, 1000], &values).unwrap();
    let mut chunks = tensor.chunks().unwrap();
    assert!(chunks.len() > 10);
    assert!(chunks.iter().all(|c| c.len() <= MAX_SINGLE_FRAME_PAYLOAD));
    chunks.reverse();
    chunks.swap(0, 5);
    assert_eq!(reassemble(&chunks).unwrap().unwrap(), tensor);
}

#[test]
fn quantization_is_four_times_smaller_and_bounded() {
    let values = ramp(10_000);
    let quantized = Tensor::quantize_i8(&[10_000], &values).unwrap();
    assert_eq!(quantized.data.len(), values.len());
    for (original, restored) in values.iter().zip(quantized.to_f32()) {
        assert!((original - restored).abs() <= quantized.scale / 2.0 + f32::EPSILON);
    }
    assert!(Tensor::quantize_i8(&[2], &[1.0, f32::NAN]).is_err());
    assert!(Tensor::quantize_i8(&[2], &[1.0, f32::INFINITY]).is_err());
    // All zeros must not divide by zero.
    assert_eq!(
        Tensor::quantize_i8(&[3], &[0.0; 3]).unwrap().to_f32(),
        [0.0; 3]
    );
}

#[test]
fn shape_must_match_data() {
    assert!(Tensor::from_f32(&[3], &[1.0, 2.0]).is_err());
    assert!(Tensor::from_f32(&[], &[]).is_err());
    assert!(Tensor::from_f32(&[0, 4], &[]).is_err());
    assert!(Tensor::from_f32(&[1; 9], &[1.0]).is_err());
    assert!(expected_len(DType::F32, &[u32::MAX; 8]).is_err());
}

#[test]
fn tampered_data_fails_the_digest() {
    let tensor = Tensor::from_f32(&[8], &ramp(8)).unwrap();
    let mut chunks = tensor.chunks().unwrap();
    let last = chunks[0].len() - 1;
    chunks[0][last] ^= 1;
    assert_eq!(
        reassemble(&chunks),
        Err(DarmError::Rejected("tensor digest mismatch".into()))
    );
}

#[test]
fn hostile_headers_are_rejected_before_buffering() {
    let tensor = Tensor::from_f32(&[8], &ramp(8)).unwrap();
    let good = tensor.chunks().unwrap().remove(0);
    let mut reassembler = Reassembler::new(ReassemblyLimits {
        max_tensor_bytes: 1 << 20,
        ..ReassemblyLimits::default()
    });

    // Truncated.
    assert!(reassembler.on_chunk(1, &good[..20], NOW).is_err());
    // Announces a tensor above the limit (total_len and shape agree).
    let big = Tensor::from_f32(&[600_000], &vec![0.0; 600_000]).unwrap();
    assert!(matches!(
        reassembler.on_chunk(1, &big.chunks().unwrap()[0], NOW),
        Err(DarmError::LimitExceeded(_))
    ));
    // total_len that disagrees with the shape.
    let mut bad = good.clone();
    bad[8..12].copy_from_slice(&64u32.to_be_bytes());
    assert!(reassembler.on_chunk(1, &bad, NOW).is_err());
    // Chunk count that disagrees with total_len.
    let mut bad = good.clone();
    bad[4..8].copy_from_slice(&9u32.to_be_bytes());
    assert!(reassembler.on_chunk(1, &bad, NOW).is_err());
    // Unknown dtype, zero dimensions, non-finite scale.
    let mut bad = good.clone();
    bad[12] = 99;
    assert!(reassembler.on_chunk(1, &bad, NOW).is_err());
    let mut bad = good.clone();
    bad[13] = 0;
    assert!(reassembler.on_chunk(1, &bad, NOW).is_err());
    let mut bad = good.clone();
    bad[18..22].copy_from_slice(&f32::NAN.to_bits().to_be_bytes());
    assert!(reassembler.on_chunk(1, &bad, NOW).is_err());

    assert_eq!(reassembler.pending_streams(), 0, "nothing was buffered");
    assert!(reassembler.on_chunk(1, &good, NOW).unwrap().is_some());
}

#[test]
fn partial_streams_are_bounded_and_expire() {
    let limits = ReassemblyLimits {
        max_streams: 2,
        timeout_ms: 100,
        ..ReassemblyLimits::default()
    };
    let mut reassembler = Reassembler::new(limits);
    let tensor = Tensor::from_f32(&[40_000], &ramp(40_000)).unwrap();
    let chunks = tensor.chunks().unwrap();
    assert!(chunks.len() >= 2);

    assert!(reassembler.on_chunk(1, &chunks[0], NOW).unwrap().is_none());
    assert!(reassembler.on_chunk(2, &chunks[0], NOW).unwrap().is_none());
    assert!(matches!(
        reassembler.on_chunk(3, &chunks[0], NOW),
        Err(DarmError::LimitExceeded(_))
    ));
    // A repeated chunk is refused but does not kill the stream.
    assert_eq!(
        reassembler.on_chunk(1, &chunks[0], NOW),
        Err(DarmError::Duplicate)
    );
    assert_eq!(reassembler.pending_streams(), 2);

    // After the timeout both partials are gone and their bytes released.
    assert!(
        reassembler
            .on_chunk(3, &chunks[0], NOW + 100)
            .unwrap()
            .is_none()
    );
    assert_eq!(reassembler.pending_streams(), 1);
    assert_eq!(
        reassembler.pending_bytes,
        chunks[0].len() - (FIXED_HEADER + 4)
    );
}

#[test]
fn metadata_change_mid_stream_discards_the_stream() {
    let a = Tensor::from_f32(&[40_000], &ramp(40_000)).unwrap();
    let b = Tensor::from_f32(&[40_000], &vec![1.0; 40_000]).unwrap();
    let mut reassembler = Reassembler::new(ReassemblyLimits::default());
    assert!(
        reassembler
            .on_chunk(1, &a.chunks().unwrap()[0], NOW)
            .unwrap()
            .is_none()
    );
    assert!(
        reassembler
            .on_chunk(1, &b.chunks().unwrap()[1], NOW)
            .is_err()
    );
    assert_eq!(reassembler.pending_streams(), 0);
    assert_eq!(reassembler.pending_bytes, 0);
}
