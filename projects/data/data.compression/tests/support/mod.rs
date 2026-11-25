//! Pure-Rust LZMA test encoder used to build synthetic streams in-code.
//!
//! The encoder mirrors the decoder model in `src/lzma` symbol for symbol
//! (same probability layout, state machine, and range coder), so the test
//! suite never depends on externally produced fixture bytes.
#![allow(dead_code)]

use std::collections::HashMap;

/// Neutral initial value of every 11-bit probability.
const PROB_INIT: u16 = 0x400;

/// Literal probabilities per context: the plain tree plus the two match-bit trees.
const LITERAL_BLOCK: usize = 0x300;

/// Range threshold below which one output byte is renormalized out.
const TOP_VALUE: u32 = 1 << 24;

/// Longest match the length model can express (high tree tops at symbol 271, plus the offset of two).
const MAX_MATCH: usize = 273;

/// Shortest match worth encoding.
const MIN_MATCH: usize = 2;

/// Byte-emitting mirror of the LZMA range decoder.
pub struct RangeEncoder {
    out: Vec<u8>,
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
}

impl RangeEncoder {
    /// Starts a fresh interval with the implicit reserved first byte pending.
    pub fn new() -> Self {
        Self { out: Vec::new(), low: 0, range: u32::MAX, cache: 0, cache_size: 1 }
    }

    /// Flushes one byte, applying the carry and the pending `0xFF` chain.
    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add((self.low >> 32) as u8));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low as u32) >> 24) as u8;
        }
        self.cache_size += 1;
        // The SDK shifts in 32-bit arithmetic (`Low = (UInt32)Low << 8`), so the
        // flushed byte is truncated away; only additions may raise the carry bit.
        self.low = u64::from((self.low as u32) << 8);
    }

    /// Renormalizes the interval by shifting one byte out when it got narrow.
    fn normalize(&mut self) {
        if self.range < TOP_VALUE {
            self.range <<= 8;
            self.shift_low();
        }
    }

    /// Encodes one bit against an adaptive probability and updates it.
    pub fn encode_bit(&mut self, prob: &mut u16, bit: u32) {
        let bound = (self.range >> 11) * u32::from(*prob);
        if bit == 0 {
            self.range = bound;
            *prob += (0x800 - *prob) >> 5;
        }
        else {
            self.low += u64::from(bound);
            self.range -= bound;
            *prob -= *prob >> 5;
        }
        self.normalize();
    }

    /// Encodes `count` uniformly distributed bits, most significant bit first.
    pub fn encode_direct(&mut self, value: u32, count: u32) {
        for index in (0..count).rev() {
            self.range >>= 1;
            if (value >> index) & 1 == 1 {
                self.low += u64::from(self.range);
            }
            self.normalize();
        }
    }

    /// Encodes a full bit tree, most significant bit first.
    pub fn encode_tree(&mut self, num_bits: u32, probs: &mut [u16], value: u32) {
        let mut node = 1usize;
        for index in (0..num_bits).rev() {
            let bit = (value >> index) & 1;
            self.encode_bit(&mut probs[node], bit);
            node = (node << 1) | bit as usize;
        }
    }

    /// Encodes a reverse bit tree, least significant bit first, at `offset`.
    pub fn encode_tree_reverse(&mut self, num_bits: u32, probs: &mut [u16], offset: usize, value: u32) {
        let mut node = 1usize;
        for index in 0..num_bits {
            let bit = (value >> index) & 1;
            self.encode_bit(&mut probs[offset + node], bit);
            node = (node << 1) | bit as usize;
        }
    }

    /// Flushes the interval and returns the encoded body.
    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

/// Match-length coder mirroring the decoder's low/mid/high trees.
#[derive(Clone)]
struct LengthModel {
    choice: u16,
    choice2: u16,
    low: [[u16; 8]; 16],
    mid: [[u16; 8]; 16],
    high: [u16; 256],
}

impl LengthModel {
    fn new() -> Self {
        Self { choice: PROB_INIT, choice2: PROB_INIT, low: [[PROB_INIT; 8]; 16], mid: [[PROB_INIT; 8]; 16], high: [PROB_INIT; 256] }
    }

    fn encode(&mut self, range: &mut RangeEncoder, value: usize, pos_state: usize) {
        if value < 8 {
            range.encode_bit(&mut self.choice, 0);
            range.encode_tree(3, &mut self.low[pos_state], value as u32);
        }
        else if value < 16 {
            range.encode_bit(&mut self.choice, 1);
            range.encode_bit(&mut self.choice2, 0);
            range.encode_tree(3, &mut self.mid[pos_state], (value - 8) as u32);
        }
        else {
            range.encode_bit(&mut self.choice, 1);
            range.encode_bit(&mut self.choice2, 1);
            range.encode_tree(8, &mut self.high, (value - 16) as u32);
        }
    }
}

/// Minimal LZMA1 encoder with a hash-chain match finder and rep0 reuse.
///
/// Only the paths the decoder must survive are exercised: literals, new
/// distances, and repeated-distance matches.
pub struct Lzma1Encoder {
    lc: u32,
    lp: u32,
    pb: u32,
    literal: Vec<u16>,
    is_match: [u16; 192],
    is_rep: [u16; 12],
    is_rep_g0: [u16; 12],
    is_rep_g1: [u16; 12],
    is_rep_g2: [u16; 12],
    is_rep_0long: [u16; 192],
    pos_slot: [[u16; 64]; 4],
    spec_pos: [u16; 115],
    align: [u16; 16],
    len: LengthModel,
    rep_len: LengthModel,
    state: usize,
    rep: [usize; 4],
    range: RangeEncoder,
    out: Vec<u8>,
    pos_base: usize,
    dict_base: usize,
    table: HashMap<u32, Vec<usize>>,
}

impl Lzma1Encoder {
    /// Creates an encoder with freshly initialized probabilities.
    pub fn new(lc: u32, lp: u32, pb: u32) -> Self {
        Self {
            lc,
            lp,
            pb,
            literal: vec![PROB_INIT; LITERAL_BLOCK << (lc + lp)],
            is_match: [PROB_INIT; 192],
            is_rep: [PROB_INIT; 12],
            is_rep_g0: [PROB_INIT; 12],
            is_rep_g1: [PROB_INIT; 12],
            is_rep_g2: [PROB_INIT; 12],
            is_rep_0long: [PROB_INIT; 192],
            pos_slot: [[PROB_INIT; 64]; 4],
            spec_pos: [PROB_INIT; 115],
            align: [PROB_INIT; 16],
            len: LengthModel::new(),
            rep_len: LengthModel::new(),
            state: 0,
            rep: [0; 4],
            range: RangeEncoder::new(),
            out: Vec::new(),
            pos_base: 0,
            dict_base: 0,
            table: HashMap::new(),
        }
    }

    /// Re-initializes probabilities, state, and the range coder, keeping output history.
    pub fn reset_model(&mut self) {
        self.literal.fill(PROB_INIT);
        self.is_match = [PROB_INIT; 192];
        self.is_rep = [PROB_INIT; 12];
        self.is_rep_g0 = [PROB_INIT; 12];
        self.is_rep_g1 = [PROB_INIT; 12];
        self.is_rep_g2 = [PROB_INIT; 12];
        self.is_rep_0long = [PROB_INIT; 192];
        self.pos_slot.fill([PROB_INIT; 64]);
        self.spec_pos = [PROB_INIT; 115];
        self.align = [PROB_INIT; 16];
        self.len = LengthModel::new();
        self.rep_len = LengthModel::new();
        self.state = 0;
        self.rep = [0; 4];
        self.range = RangeEncoder::new();
        self.pos_base = self.out.len();
    }

    /// Forgets output older than the current length (dictionary reset).
    pub fn reset_dictionary(&mut self) {
        self.dict_base = self.out.len();
    }

    /// Adds externally produced bytes (uncompressed chunks) to the history.
    pub fn push_history(&mut self, bytes: &[u8]) {
        for index in 0..bytes.len() {
            let position = self.out.len() + index;
            if bytes.len() - index >= 4 {
                let key = u32::from_le_bytes([bytes[index], bytes[index + 1], bytes[index + 2], bytes[index + 3]]);
                self.table.entry(key).or_default().push(position);
                let bucket = self.table.get_mut(&key).expect("bucket just inserted");
                if bucket.len() > 4 {
                    bucket.remove(0);
                }
            }
        }
        self.out.extend_from_slice(bytes);
    }

    /// Encodes one slice of input, appending to the shared output history.
    pub fn encode(&mut self, chunk: &[u8]) {
        let mut index = 0;
        while index < chunk.len() {
            let position = self.out.len();
            let pos_state = (position - self.pos_base) & ((1usize << self.pb) - 1);
            let rep_length = self.match_length(self.rep[0] + 1, &chunk[index..]);
            let (length, distance) = self.find_match(&chunk[index..]);
            if rep_length >= MIN_MATCH && rep_length >= length {
                self.encode_rep0(chunk, index, rep_length, pos_state);
                index += rep_length;
                continue;
            }
            if length >= MIN_MATCH {
                self.encode_match(chunk, index, length, distance, pos_state);
                index += length;
                continue;
            }
            self.encode_literal(chunk, index, pos_state);
            index += 1;
        }
    }

    /// Emits the unknown-length end marker (new distance `0xFFFF_FFFF`).
    pub fn encode_end_marker(&mut self) {
        let pos_state = (self.out.len() - self.pos_base) & ((1usize << self.pb) - 1);
        self.range.encode_bit(&mut self.is_match[(self.state << 4) + pos_state], 1);
        self.range.encode_bit(&mut self.is_rep[self.state], 0);
        self.len.encode(&mut self.range, 0, pos_state);
        self.encode_distance(0xFFFF_FFFF, 0);
    }

    /// Flushes the current chunk body and starts a fresh range coder.
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::replace(&mut self.range, RangeEncoder::new()).finish()
    }

    /// Encodes one literal, mirroring the decoder's match-byte model usage.
    fn encode_literal(&mut self, chunk: &[u8], index: usize, pos_state: usize) {
        let byte = chunk[index];
        let position = self.out.len();
        let previous = if position == self.pos_base { 0u8 } else { self.out[position - 1] };
        let context = (((position - self.pos_base) & ((1usize << self.lp) - 1)) << self.lc) + (usize::from(previous) >> (8 - self.lc));
        let base = context * LITERAL_BLOCK;
        self.range.encode_bit(&mut self.is_match[(self.state << 4) + pos_state], 0);

        let mut symbol = 1usize;
        let mut remaining = 8usize;
        if self.state >= 7 {
            let distance = self.rep[0] + 1;
            let mut match_byte = usize::from(self.out[position - distance]);
            while symbol < 0x100 {
                let bit = ((byte >> (remaining - 1)) & 1) as usize;
                let match_bit = (match_byte >> 7) & 1;
                self.range.encode_bit(&mut self.literal[base + ((1 + match_bit) << 8) + symbol], bit as u32);
                symbol = (symbol << 1) | bit;
                remaining -= 1;
                match_byte = (match_byte << 1) & 0x1FF;
                if match_bit != bit {
                    break;
                }
            }
        }
        while symbol < 0x100 {
            let bit = ((byte >> (remaining - 1)) & 1) as usize;
            self.range.encode_bit(&mut self.literal[base + symbol], bit as u32);
            symbol = (symbol << 1) | bit;
            remaining -= 1;
        }

        self.state = if self.state < 4 {
            0
        }
        else if self.state < 10 {
            self.state - 3
        }
        else {
            self.state - 6
        };
        self.push_output(byte);
    }

    /// Encodes a match with a fresh distance.
    fn encode_match(&mut self, chunk: &[u8], index: usize, length: usize, distance: usize, pos_state: usize) {
        self.range.encode_bit(&mut self.is_match[(self.state << 4) + pos_state], 1);
        self.range.encode_bit(&mut self.is_rep[self.state], 0);
        self.len.encode(&mut self.range, length - 2, pos_state);
        self.encode_distance(distance - 1, length - 2);
        self.state = if self.state < 7 { 7 } else { 10 };
        self.rep[3] = self.rep[2];
        self.rep[2] = self.rep[1];
        self.rep[1] = self.rep[0];
        self.rep[0] = distance - 1;
        for offset in 0..length {
            let byte = chunk[index + offset];
            self.push_output(byte);
        }
    }

    /// Encodes a match reusing the most recent distance.
    fn encode_rep0(&mut self, chunk: &[u8], index: usize, length: usize, pos_state: usize) {
        self.range.encode_bit(&mut self.is_match[(self.state << 4) + pos_state], 1);
        self.range.encode_bit(&mut self.is_rep[self.state], 1);
        self.range.encode_bit(&mut self.is_rep_g0[self.state], 0);
        self.range.encode_bit(&mut self.is_rep_0long[(self.state << 4) + pos_state], 1);
        self.rep_len.encode(&mut self.range, length - 2, pos_state);
        self.state = if self.state < 7 { 8 } else { 11 };
        for offset in 0..length {
            let byte = chunk[index + offset];
            self.push_output(byte);
        }
    }

    /// Encodes a zero-based distance through slot, direct, and align models.
    fn encode_distance(&mut self, distance: usize, length_symbol: usize) {
        let len_state = if length_symbol > 3 { 3 } else { length_symbol };
        let slot = if distance < 4 {
            distance
        }
        else {
            let bits = usize::try_from(32 - u32::try_from(distance).expect("distance fits a u32").leading_zeros()).expect("slot index fits");
            ((bits - 1) << 1) | ((distance >> (bits - 2)) & 1)
        };
        self.range.encode_tree(6, &mut self.pos_slot[len_state], slot as u32);
        if slot < 4 {
            return;
        }
        let direct_bits = (slot >> 1) - 1;
        let base = (2 | (slot & 1)) << direct_bits;
        let value = distance - base;
        if slot < 14 {
            self.range.encode_tree_reverse(direct_bits as u32, &mut self.spec_pos, base - slot, value as u32);
        }
        else {
            self.range.encode_direct((value >> 4) as u32, (direct_bits - 4) as u32);
            self.range.encode_tree_reverse(4, &mut self.align, 0, (value & 0xF) as u32);
        }
    }

    /// Longest match of `rest` against history at back-distance `distance`.
    fn match_length(&self, distance: usize, rest: &[u8]) -> usize {
        if distance > self.out.len() - self.dict_base {
            return 0;
        }
        let mut length = 0;
        while length < MAX_MATCH && length < rest.len() {
            let source = if length < distance { self.out[self.out.len() - distance + length] } else { rest[length - distance] };
            if source != rest[length] {
                break;
            }
            length += 1;
        }
        length
    }

    /// Hash-indexed greedy search for the best new distance.
    fn find_match(&mut self, rest: &[u8]) -> (usize, usize) {
        if rest.len() < MIN_MATCH {
            return (0, 0);
        }
        let position = self.out.len();
        let mut best = (0usize, 0usize);
        if rest.len() >= 4 {
            let key = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
            if let Some(candidates) = self.table.get(&key).cloned() {
                for candidate in candidates {
                    let distance = position - candidate;
                    let length = self.match_length(distance, rest);
                    if length > best.0 {
                        best = (length, distance);
                    }
                }
            }
            self.table.entry(key).or_default().push(position);
            let bucket = self.table.get_mut(&key).expect("bucket just inserted");
            if bucket.len() > 4 {
                bucket.remove(0);
            }
        }
        best
    }

    /// Appends one produced byte to the shared history.
    fn push_output(&mut self, byte: u8) {
        self.out.push(byte);
    }
}

/// Encodes the single property byte from `lc`/`lp`/`pb`.
pub fn properties_byte(lc: u32, lp: u32, pb: u32) -> u8 {
    (lc + lp * 9 + pb * 45) as u8
}

/// Builds a complete `LZMA_ALONE` stream from `data`.
pub fn alone(data: &[u8], lc: u32, lp: u32, pb: u32, dictionary: u32, known_size: bool) -> Vec<u8> {
    let mut encoder = Lzma1Encoder::new(lc, lp, pb);
    encoder.encode(data);
    let body = if known_size {
        encoder.take_body()
    }
    else {
        encoder.encode_end_marker();
        encoder.take_body()
    };
    let mut stream = vec![properties_byte(lc, lp, pb)];
    stream.extend(dictionary.to_le_bytes());
    stream.extend(if known_size { (data.len() as u64).to_le_bytes() } else { u64::MAX.to_le_bytes() });
    stream.extend(body);
    stream
}

/// Appends an LZMA2 uncompressed chunk (`0x01` with dictionary reset, `0x02` without).
pub fn lzma2_uncompressed(bytes: &mut Vec<u8>, data: &[u8], dictionary_reset: bool) {
    bytes.push(if dictionary_reset { 0x01 } else { 0x02 });
    bytes.extend(((data.len() - 1) as u16).to_be_bytes());
    bytes.extend_from_slice(data);
}

/// Appends an LZMA2 LZMA chunk carrying the body produced by `encoder`.
pub fn lzma2_chunk(bytes: &mut Vec<u8>, encoder: &mut Lzma1Encoder, chunk: &[u8], reset: u8, properties: Option<u8>) {
    encoder.encode(chunk);
    let body = encoder.take_body();
    let unpacked = chunk.len() - 1;
    bytes.push(0x80 | (reset << 5) | (unpacked >> 16) as u8);
    bytes.extend(((unpacked & 0xFFFF) as u16).to_be_bytes());
    bytes.extend(((body.len() - 1) as u16).to_be_bytes());
    if let Some(byte) = properties {
        bytes.push(byte);
    }
    bytes.extend(body);
}

/// Appends the LZMA2 end-of-stream control byte.
pub fn lzma2_end(bytes: &mut Vec<u8>) {
    bytes.push(0x00);
}
