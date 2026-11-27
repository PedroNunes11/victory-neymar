//! Content hashes used only to decide whether title texture memory changed.

use std::sync::OnceLock;

/// Uses the faster non-cryptographic content hash when explicitly enabled.
///
/// The choice is fixed for the process, so hashes stored by a staging
/// texture are always compared with hashes made by the same algorithm.
fn fast() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_FAST_HASH").is_some_and(|v| v == "1"))
}

pub(crate) fn content_hash(bytes: &[u8]) -> blake3::Hash {
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let hash = if fast() {
        blake3::Hash::from(lane_hash(bytes))
    } else {
        blake3::hash(bytes)
    };
    #[cfg(feature = "diagnostics")]
    crate::texture::timed(1, started, bytes.len());
    hash
}

const LANES: usize = 8;
const K: [u64; LANES] = [
    0x9e37_79b9_7f4a_7c15,
    0xc2b2_ae3d_27d4_eb4f,
    0x1656_67b1_9e37_79f9,
    0x85eb_ca77_c2b2_ae63,
    0x27d4_eb2f_1656_67c5,
    0xff51_afd7_ed55_8ccd,
    0xc4ce_b9fe_1a85_ec53,
    0xd6e8_feb8_6659_fd93,
];

/// A memory-speed 256-bit change detector: eight independent word lanes,
/// followed by a finish that mixes every lane into every output word.
fn lane_hash(bytes: &[u8]) -> [u8; 32] {
    let mut lane = K;
    let mut blocks = bytes.chunks_exact(8 * LANES);
    for block in &mut blocks {
        for (i, state) in lane.iter_mut().enumerate() {
            let word = u64::from_le_bytes(block[8 * i..8 * i + 8].try_into().unwrap());
            *state = (state.rotate_left(23) ^ word).wrapping_mul(K[i] | 1);
        }
    }
    for (i, word) in blocks.remainder().chunks(8).enumerate() {
        let mut padded = [0u8; 8];
        padded[..word.len()].copy_from_slice(word);
        lane[i] = (lane[i].rotate_left(23) ^ u64::from_le_bytes(padded)).wrapping_mul(K[i] | 1);
    }
    lane[0] ^= bytes.len() as u64;

    let mut out = [0u8; 32];
    for output in 0..4 {
        let mut mixed = (output as u64).wrapping_mul(K[7]) ^ bytes.len() as u64;
        for (i, state) in lane.iter().enumerate() {
            mixed = (mixed ^ state.rotate_left((7 * i + 13 * output) as u32 % 64))
                .wrapping_mul(K[(i + output) % LANES] | 1);
            mixed ^= mixed >> 29;
        }
        out[8 * output..8 * output + 8].copy_from_slice(&mixed.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::lane_hash;

    #[test]
    fn every_one_byte_change_is_seen() {
        let base = vec![0u8; 257];
        let expected = lane_hash(&base);
        for at in 0..base.len() {
            let mut changed = base.clone();
            changed[at] = 1;
            assert_ne!(lane_hash(&changed), expected, "byte {at}");
        }
    }

    #[test]
    fn length_and_word_order_are_seen() {
        let a: Vec<u8> = (0..130).map(|n| n as u8).collect();
        let mut b = a.clone();
        let first: [u8; 8] = b[8..16].try_into().unwrap();
        let second: [u8; 8] = b[64..72].try_into().unwrap();
        b[8..16].copy_from_slice(&second);
        b[64..72].copy_from_slice(&first);
        assert_ne!(lane_hash(&a), lane_hash(&b));
        assert_ne!(lane_hash(&a), lane_hash(&a[..a.len() - 1]));
    }

    #[test]
    #[ignore = "offline performance measurement"]
    fn compare_with_blake3() {
        let bytes: Vec<u8> = (0usize..16 * 1024 * 1024)
            .map(|n| (n.wrapping_mul(31) ^ (n >> 7)) as u8)
            .collect();
        let rounds = 16;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(blake3::hash(&bytes));
        }
        let blake3 = started.elapsed();
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(lane_hash(&bytes));
        }
        let lane = started.elapsed();
        println!(
            "hashed {} MiB: BLAKE3 {blake3:?}, lane {lane:?}, {:.2}x faster",
            bytes.len() * rounds / 1024 / 1024,
            blake3.as_secs_f64() / lane.as_secs_f64()
        );
    }
}
