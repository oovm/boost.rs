//! Seed-zero XXH64 for content fingerprints and Zstandard trailers.
//!
//! This replaces the former external `xxhash` dependency. Only the streaming
//! 64-bit variant with seed `0` is implemented, matching the reference
//! algorithm byte-for-byte.

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline]
fn round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

#[inline]
fn merge(acc: u64, lane: u64) -> u64 {
    (acc ^ round(0, lane)).wrapping_mul(P1).wrapping_add(P4)
}

#[inline]
fn read_u64(input: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(input[at..at + 8].try_into().expect("slice of length 8"))
}

#[inline]
fn read_u32(input: &[u8], at: usize) -> u64 {
    u32::from_le_bytes(input[at..at + 4].try_into().expect("slice of length 4")).into()
}

/// Streaming XXH64 hasher with the reference algorithm's exact output.
#[derive(Clone, Debug)]
pub struct XxHash64 {
    acc: [u64; 4],
    buffer: [u8; 32],
    buffered: usize,
    total_len: u64,
}

impl Default for XxHash64 {
    fn default() -> Self {
        Self { acc: [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)], buffer: [0; 32], buffered: 0, total_len: 0 }
    }
}

impl XxHash64 {
    /// Creates a hasher with seed `0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes into the streaming hash.
    pub fn write(&mut self, mut input: &[u8]) {
        self.total_len = self.total_len.wrapping_add(input.len() as u64);

        if self.buffered + input.len() < 32 {
            self.buffer[self.buffered..self.buffered + input.len()].copy_from_slice(input);
            self.buffered += input.len();
            return;
        }

        if self.buffered > 0 {
            let fill = 32 - self.buffered;
            self.buffer[self.buffered..].copy_from_slice(&input[..fill]);
            let block = self.buffer;
            self.process_block(&block);
            input = &input[fill..];
            self.buffered = 0;
        }

        while input.len() >= 32 {
            self.process_block(&input[..32]);
            input = &input[32..];
        }

        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.buffered = input.len();
        }
    }

    fn process_block(&mut self, block: &[u8]) {
        self.acc[0] = round(self.acc[0], read_u64(block, 0));
        self.acc[1] = round(self.acc[1], read_u64(block, 8));
        self.acc[2] = round(self.acc[2], read_u64(block, 16));
        self.acc[3] = round(self.acc[3], read_u64(block, 24));
    }

    /// Finalizes and returns the 64-bit digest.
    pub fn finish(&self) -> u64 {
        let mut h = if self.total_len >= 32 {
            self.acc[0]
                .rotate_left(1)
                .wrapping_add(self.acc[1].rotate_left(7).wrapping_add(self.acc[2].rotate_left(12).wrapping_add(self.acc[3].rotate_left(18))))
                .wrapping_mul(P1)
                .wrapping_add(
                    merge(0, self.acc[0])
                        .wrapping_add(merge(0, self.acc[1]))
                        .wrapping_add(merge(0, self.acc[2]))
                        .wrapping_add(merge(0, self.acc[3])),
                )
        }
        else {
            P5
        };
        h = h.wrapping_add(self.total_len);

        let tail = &self.buffer[..self.buffered];
        let mut i = 0usize;
        while i + 8 <= tail.len() {
            h = (h ^ round(0, read_u64(tail, i))).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
            i += 8;
        }
        if i + 4 <= tail.len() {
            h = (h ^ (read_u32(tail, i).wrapping_mul(P1))).rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
            i += 4;
        }
        while i < tail.len() {
            h = (h ^ (tail[i] as u64).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
            i += 1;
        }

        h ^= h >> 33;
        h = h.wrapping_mul(P2);
        h ^= h >> 29;
        h = h.wrapping_mul(P3);
        h ^= h >> 32;
        h
    }
}

#[cfg(test)]
mod tests {
    use super::XxHash64;

    fn digest(input: &[u8]) -> u64 {
        let mut hasher = XxHash64::new();
        hasher.write(input);
        hasher.finish()
    }

    #[test]
    fn empty_input_matches_reference_vector() {
        assert_eq!(digest(b""), 0xEF46_DB37_51D8_E999);
    }

    #[test]
    fn single_byte_matches_reference_vector() {
        assert_eq!(digest(b"a"), 0xD24E_C4F1_A98C_6E5B);
    }

    #[test]
    fn short_input_matches_reference_vector() {
        assert_eq!(digest(b"abc"), 0x44BC_2CF5_AD77_0999);
    }

    #[test]
    fn four_byte_input_matches_reference_vector() {
        assert_eq!(digest(b"abcd"), 0xDE03_27B0_D25D_92CC);
    }

    #[test]
    fn long_input_matches_reference_vector() {
        assert_eq!(digest(&[7u8; 100]), 0x9AD4_1D99_184E_1438);
        assert_eq!(digest(b"XXH64 hash algorithm test vector with more than 32 bytes!"), 0xD748_7E2A_526A_19DB);
    }

    #[test]
    fn long_input_is_streaming_invariant() {
        let whole = digest(&[7u8; 100]);
        let mut hasher = XxHash64::new();
        hasher.write(&[7u8; 3]);
        hasher.write(&[7u8; 29]);
        hasher.write(&[7u8; 68]);
        assert_eq!(hasher.finish(), whole);
    }
}
