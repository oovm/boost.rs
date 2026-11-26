//! The RFC 7932 static dictionary and its word transforms.

use super::{
    BrotliError,
    tables::{PREFIX_SUFFIX, TRANSFORMS},
};

/// Raw static dictionary data.
///
/// Provenance and license
/// ---------------------
///
/// This 122784-byte blob is the static dictionary specified by RFC 7932,
/// section 8. It is not creative content of this repository: it is the
/// reference word list defined by the format.
///
/// - Source: extracted verbatim from the MIT-licensed `brotli-decompressor`
///   5.0.3 sources (`src/dictionary/mod.rs`), which store the same bytes as a
///   byte array.
/// - Size: 122784 bytes.
/// - SHA-256: `20e42eb1b511c21806d4d227d07e5dd06877d8ce7b3a817f378f313653f35c70`.
/// - The tables in [`super::tables`] are transcribed from RFC 7932 (matching
///   the reference tables shipped by `brotli-decompressor`).
///
/// No code from `brotli-decompressor` is used at runtime; the unit tests below
/// pin the length and digest so the blob cannot change silently.
const DICTIONARY: &[u8; 122784] = include_bytes!("dictionary.bin");

/// Start offset of the words of each length inside [`DICTIONARY`].
const OFFSETS_BY_LENGTH: [u32; 25] = [
    0, 0, 0, 0, 0, 4096, 9216, 21504, 35840, 44032, 53248, 63488, 74752, 87040, 93696, 100864, 104704, 106752, 108928, 113536, 115968, 118528,
    119872, 121280, 122016,
];

/// Number of index bits for the words of each length.
const SIZE_BITS_BY_LENGTH: [u8; 25] = [0, 0, 0, 0, 10, 10, 11, 11, 10, 10, 10, 10, 10, 9, 9, 8, 7, 7, 8, 7, 7, 6, 6, 5, 5];

/// Shortest word that can be referenced from the static dictionary.
pub(crate) const MIN_WORD_LENGTH: usize = 4;

/// Longest word that can be referenced from the static dictionary.
pub(crate) const MAX_WORD_LENGTH: usize = 24;

/// Looks up and transforms one dictionary word.
///
/// `word_id` encodes both the word index and the transform; the transform
/// identifier occupies the high bits above the per-length index width. Returns
/// [`BrotliError::Corrupt`] when the length or index is outside the dictionary.
pub(crate) fn word(length: usize, word_id: u32) -> Result<Vec<u8>, BrotliError> {
    if !(MIN_WORD_LENGTH..=MAX_WORD_LENGTH).contains(&length) {
        return Err(BrotliError::Corrupt("dictionary reference with bad length"));
    }
    let shift = u32::from(SIZE_BITS_BY_LENGTH[length]);
    let index = (word_id & ((1u32 << shift) - 1)) as usize;
    let transform = (word_id >> shift) as usize;
    if transform >= TRANSFORMS.len() {
        return Err(BrotliError::Corrupt("dictionary transform out of range"));
    }
    let offset = OFFSETS_BY_LENGTH[length] as usize + index * length;
    let end = offset + length;
    if end > DICTIONARY.len() {
        return Err(BrotliError::Corrupt("dictionary index out of range"));
    }
    apply_transform(transform, &DICTIONARY[offset..end])
}

/// Applies the transform `transform_id` to `word`.
///
/// Returns [`BrotliError::Corrupt`] when `transform_id` is not one of the 121
/// transforms defined by RFC 7932.
pub(crate) fn apply_transform(transform_id: usize, word: &[u8]) -> Result<Vec<u8>, BrotliError> {
    if transform_id >= TRANSFORMS.len() {
        return Err(BrotliError::Corrupt("dictionary transform out of range"));
    }
    let transform = TRANSFORMS[transform_id];
    let operation = transform.transform;
    let mut out = Vec::with_capacity(word.len() + 8);
    push_prefix_suffix(&mut out, transform.prefix_id);

    // Operations 12..=20 drop the first (operation - 11) bytes.
    let skip = if operation >= 12 { usize::from(operation - 11).min(word.len()) } else { 0 };
    let mut body = &word[skip..];
    // Operations 1..=9 drop that many trailing bytes.
    if operation <= 9 {
        body = &body[..body.len().saturating_sub(usize::from(operation))];
    }
    let start = out.len();
    out.extend_from_slice(body);
    match operation {
        // 10: uppercase the first character only.
        10 => uppercase_first(&mut out[start..]),
        // 11: uppercase every character.
        11 => uppercase_all(&mut out[start..]),
        _ => {}
    }
    push_prefix_suffix(&mut out, transform.suffix_id);
    Ok(out)
}

/// Appends the NUL-terminated prefix or suffix string starting at `offset`.
fn push_prefix_suffix(out: &mut Vec<u8>, offset: u8) {
    let mut index = usize::from(offset);
    while let Some(&byte) = PREFIX_SUFFIX.get(index) {
        if byte == 0 {
            break;
        }
        out.push(byte);
        index += 1;
    }
}

/// Uppercases the first character, using the simplified UTF-8 model of the spec.
fn uppercase_first(data: &mut [u8]) {
    uppercase_step(data);
}

/// Uppercases every character, using the simplified UTF-8 model of the spec.
fn uppercase_all(data: &mut [u8]) {
    let mut offset = 0;
    while offset < data.len() {
        offset += uppercase_step(&mut data[offset..]);
    }
}

/// Uppercases one character and returns its encoded length in bytes.
fn uppercase_step(data: &mut [u8]) -> usize {
    if data.is_empty() {
        return 1;
    }
    let first = data[0];
    if first < 0xc0 {
        if first.is_ascii_lowercase() {
            data[0] ^= 0x20;
        }
        1
    }
    else if first < 0xe0 {
        if let Some(second) = data.get_mut(1) {
            *second ^= 0x20;
        }
        2
    }
    else {
        if let Some(third) = data.get_mut(2) {
            *third ^= 5;
        }
        3
    }
}

#[cfg(test)]
mod tests {
    use super::DICTIONARY;

    /// SHA-256 of the RFC 7932 static dictionary.
    const EXPECTED_SHA256: [u8; 32] = [
        0x20, 0xe4, 0x2e, 0xb1, 0xb5, 0x11, 0xc2, 0x18, 0x06, 0xd4, 0xd2, 0x27, 0xd0, 0x7e, 0x5d, 0xd0, 0x68, 0x77, 0xd8, 0xce, 0x7b, 0x3a,
        0x81, 0x7f, 0x37, 0x8f, 0x31, 0x36, 0x53, 0xf3, 0x5c, 0x70,
    ];

    #[test]
    fn dictionary_blob_is_the_spec_dictionary() {
        assert_eq!(DICTIONARY.len(), 122784, "dictionary length drifted from RFC 7932");
        assert_eq!(sha256(DICTIONARY), EXPECTED_SHA256, "dictionary bytes drifted from RFC 7932");
    }

    /// Minimal SHA-256 so the test pins the blob without a runtime dependency.
    fn sha256(data: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a_2f98,
            0x7137_4491,
            0xb5c0_fbcf,
            0xe9b5_dba5,
            0x3956_c25b,
            0x59f1_11f1,
            0x923f_82a4,
            0xab1c_5ed5,
            0xd807_aa98,
            0x1283_5b01,
            0x2431_85be,
            0x550c_7dc3,
            0x72be_5d74,
            0x80de_b1fe,
            0x9bdc_06a7,
            0xc19b_f174,
            0xe49b_69c1,
            0xefbe_4786,
            0x0fc1_9dc6,
            0x240c_a1cc,
            0x2de9_2c6f,
            0x4a74_84aa,
            0x5cb0_a9dc,
            0x76f9_88da,
            0x983e_5152,
            0xa831_c66d,
            0xb003_27c8,
            0xbf59_7fc7,
            0xc6e0_0bf3,
            0xd5a7_9147,
            0x06ca_6351,
            0x1429_2967,
            0x27b7_0a85,
            0x2e1b_2138,
            0x4d2c_6dfc,
            0x5338_0d13,
            0x650a_7354,
            0x766a_0abb,
            0x81c2_c92e,
            0x9272_2c85,
            0xa2bf_e8a1,
            0xa81a_664b,
            0xc24b_8b70,
            0xc76c_51a3,
            0xd192_e819,
            0xd699_0624,
            0xf40e_3585,
            0x106a_a070,
            0x19a4_c116,
            0x1e37_6c08,
            0x2748_774c,
            0x34b0_bcb5,
            0x391c_0cb3,
            0x4ed8_aa4a,
            0x5b9c_ca4f,
            0x682e_6ff3,
            0x748f_82ee,
            0x78a5_636f,
            0x84c8_7814,
            0x8cc7_0208,
            0x90be_fffa,
            0xa450_6ceb,
            0xbef9_a3f7,
            0xc671_78f2,
        ];
        let mut state: [u32; 8] = [0x6a09_e667, 0xbb67_ae85, 0x3c6e_f372, 0xa54f_f53a, 0x510e_527f, 0x9b05_688c, 0x1f83_d9ab, 0x5be0_cd19];
        let bit_length = (data.len() as u64) * 8;
        let mut message = data.to_vec();
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_length.to_be_bytes());
        for chunk in message.chunks_exact(64) {
            let mut w = [0u32; 64];
            for (index, word) in w.iter_mut().take(16).enumerate() {
                *word = u32::from_be_bytes([chunk[index * 4], chunk[index * 4 + 1], chunk[index * 4 + 2], chunk[index * 4 + 3]]);
            }
            for index in 16..64 {
                let s0 = w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
                let s1 = w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
                w[index] = w[index - 16].wrapping_add(s0).wrapping_add(w[index - 7]).wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
            for index in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let choose = (e & f) ^ (!e & g);
                let t1 = h.wrapping_add(s1).wrapping_add(choose).wrapping_add(K[index]).wrapping_add(w[index]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let majority = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(majority);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
                *slot = slot.wrapping_add(value);
            }
        }
        let mut digest = [0u8; 32];
        for (index, word) in state.iter().enumerate() {
            digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }
}
