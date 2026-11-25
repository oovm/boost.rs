//! LZMA1 arithmetic state machine shared by the LZMA1 and LZMA2 codecs.
//!
//! The model layout follows the reference LZMA specification: twelve states,
//! four most-recent distances, literal contexts selected by `lc`/`lp`, and
//! distance contexts selected by `pb`. Probability blocks are kept separate
//! per model instead of one flat arena, so every index is local and auditable.

use super::range::RangeDecoder;
use crate::{CodecError, CodecLimits, bit::BitPosition};

/// Neutral initial value of every 11-bit probability.
const PROB_INIT: u16 = 0x400;

/// Literal probabilities per context: the plain tree plus the two match-bit trees.
const LITERAL_BLOCK: usize = 0x300;

/// Maximum sum of `lc` and `lp` accepted by the reference model.
const MAX_LITERAL_CONTEXTS: u32 = 4;

/// Parsed `lc`/`lp`/`pb` property byte of an LZMA stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LzmaProperties {
    /// Literal context bits taken from the previous byte.
    pub(crate) lc: u32,
    /// Literal context bits taken from the output position.
    pub(crate) lp: u32,
    /// Match/length position bits.
    pub(crate) pb: u32,
}

impl LzmaProperties {
    /// Decodes the single property byte, rejecting out-of-range combinations.
    pub(crate) fn from_byte(byte: u8, offset: usize) -> Result<Self, CodecError> {
        let mut value = u32::from(byte);
        if value >= 225 {
            return Err(CodecError::InvalidByte { offset, byte });
        }
        let lc = value % 9;
        value /= 9;
        let lp = value % 5;
        value /= 5;
        if lc + lp > MAX_LITERAL_CONTEXTS {
            return Err(CodecError::InvalidByte { offset, byte });
        }
        Ok(Self { lc, lp, pb: value })
    }
}

/// Match-length coder: low and mid trees indexed by position state, plus a high tree.
#[derive(Debug)]
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

    fn reset(&mut self) {
        self.choice = PROB_INIT;
        self.choice2 = PROB_INIT;
        self.low.fill([PROB_INIT; 8]);
        self.mid.fill([PROB_INIT; 8]);
        self.high = [PROB_INIT; 256];
    }

    /// Decodes a match length already offset by two (the minimum LZMA match).
    fn decode(&mut self, range: &mut RangeDecoder<'_>, pos_state: usize) -> Result<usize, CodecError> {
        if range.decode_bit(&mut self.choice)? == 0 {
            Ok(range.decode_tree(3, &mut self.low[pos_state])? as usize)
        }
        else if range.decode_bit(&mut self.choice2)? == 0 {
            Ok(range.decode_tree(3, &mut self.mid[pos_state])? as usize + 8)
        }
        else {
            Ok(range.decode_tree(8, &mut self.high)? as usize + 16)
        }
    }
}

/// Output window and stop condition for one decode pass.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DecodeScope {
    /// Output length where the position-state counter and the literal context restart.
    pub(crate) pos_base: usize,
    /// Oldest output byte a match may reference.
    pub(crate) dict_base: usize,
    /// Maximum reachable back-reference distance.
    pub(crate) window: usize,
    /// Byte count to stop at; `None` decodes until the input is exhausted.
    pub(crate) target: Option<usize>,
}

/// Full LZMA1 decoder state: probabilities, state machine, and distance LRU.
#[derive(Debug)]
pub(crate) struct LzmaCore {
    /// Active `lc`/`lp`/`pb` properties.
    properties: LzmaProperties,
    /// Literal probability blocks, `1 << (lc + lp)` contexts of [`LITERAL_BLOCK`].
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
}

impl LzmaCore {
    /// Creates a freshly initialized model for the given properties.
    pub(crate) fn new(properties: LzmaProperties) -> Self {
        Self {
            properties,
            literal: vec![PROB_INIT; LITERAL_BLOCK << (properties.lc + properties.lp)],
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
        }
    }

    /// Active `lc`/`lp`/`pb` properties of the model.
    pub(crate) fn properties(&self) -> LzmaProperties {
        self.properties
    }

    /// Replaces the properties and resets every probability, state, and distance.
    pub(crate) fn reset(&mut self, properties: LzmaProperties) {
        if properties.lc + properties.lp != self.properties.lc + self.properties.lp {
            self.literal = vec![PROB_INIT; LITERAL_BLOCK << (properties.lc + properties.lp)];
        }
        else {
            self.literal.fill(PROB_INIT);
        }
        self.properties = properties;
        self.is_match = [PROB_INIT; 192];
        self.is_rep = [PROB_INIT; 12];
        self.is_rep_g0 = [PROB_INIT; 12];
        self.is_rep_g1 = [PROB_INIT; 12];
        self.is_rep_g2 = [PROB_INIT; 12];
        self.is_rep_0long = [PROB_INIT; 192];
        self.pos_slot.fill([PROB_INIT; 64]);
        self.spec_pos = [PROB_INIT; 115];
        self.align = [PROB_INIT; 16];
        self.len.reset();
        self.rep_len.reset();
        self.state = 0;
        self.rep = [0; 4];
    }

    /// Decodes symbols into `out` until the scope's target is reached.
    ///
    /// An absent target keeps decoding until the range coder runs out of
    /// input bytes, which is how unknown-length LZMA1 streams end.
    pub(crate) fn decode(
        &mut self,
        range: &mut RangeDecoder<'_>,
        out: &mut Vec<u8>,
        scope: DecodeScope,
        limits: &CodecLimits,
    ) -> Result<(), CodecError> {
        loop {
            if let Some(target) = scope.target
                && out.len() >= target
            {
                return Ok(());
            }
            match self.decode_symbol(range, out, scope, limits) {
                Ok(true) => continue,
                Ok(false) | Err(CodecError::InputExhausted { .. }) if scope.target.is_none() => return Ok(()),
                Ok(false) => {
                    return Err(CodecError::InputExhausted { offset: range.consumed(), needed: scope.target.unwrap_or(0) - out.len() });
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Decodes exactly one literal or match symbol.
    ///
    /// Returns `false` when the unknown-length end-of-stream marker (a new
    /// distance of `0xFFFF_FFFF`) appears, signaling that no more output
    /// follows.
    fn decode_symbol(
        &mut self,
        range: &mut RangeDecoder<'_>,
        out: &mut Vec<u8>,
        scope: DecodeScope,
        limits: &CodecLimits,
    ) -> Result<bool, CodecError> {
        let pos_mask = (1usize << self.properties.pb) - 1;
        let pos_state = (out.len() - scope.pos_base) & pos_mask;
        if range.decode_bit(&mut self.is_match[(self.state << 4) + pos_state])? == 0 {
            let byte = self.decode_literal(range, out, scope)?;
            self.state = if self.state < 4 {
                0
            }
            else if self.state < 10 {
                self.state - 3
            }
            else {
                self.state - 6
            };
            self.reserve(out, 1, limits)?;
            out.push(byte);
            return Ok(true);
        }

        if range.decode_bit(&mut self.is_rep[self.state])? == 1 {
            if range.decode_bit(&mut self.is_rep_g0[self.state])? == 1 {
                let index = if range.decode_bit(&mut self.is_rep_g1[self.state])? == 0 {
                    1
                }
                else if range.decode_bit(&mut self.is_rep_g2[self.state])? == 0 {
                    2
                }
                else {
                    3
                };
                let distance = self.rep[index];
                self.rep.copy_within(0..index, 1);
                self.rep[0] = distance;
            }
            else if range.decode_bit(&mut self.is_rep_0long[(self.state << 4) + pos_state])? == 0 {
                self.state = if self.state < 7 { 9 } else { 11 };
                self.copy_match(out, 1, self.rep[0] + 1, scope, limits)?;
                return Ok(true);
            }
            let len = self.rep_len.decode(range, pos_state)?;
            self.state = if self.state < 7 { 8 } else { 11 };
            self.copy_match(out, len + 2, self.rep[0] + 1, scope, limits)?;
            return Ok(true);
        }

        let len = self.len.decode(range, pos_state)?;
        let distance = self.decode_distance(range, len)?;
        if distance == u32::MAX as usize {
            return Ok(false);
        }
        self.state = if self.state < 7 { 7 } else { 10 };
        self.rep[3] = self.rep[2];
        self.rep[2] = self.rep[1];
        self.rep[1] = self.rep[0];
        self.rep[0] = distance;
        self.copy_match(out, len + 2, distance + 1, scope, limits)?;
        Ok(true)
    }

    /// Decodes one literal byte using the `lc`/`lp` context and, inside match
    /// states, the match-byte model.
    fn decode_literal(&mut self, range: &mut RangeDecoder<'_>, out: &[u8], scope: DecodeScope) -> Result<u8, CodecError> {
        let previous = if out.len() == scope.pos_base { 0u8 } else { out[out.len() - 1] };
        let context = (((out.len() - scope.pos_base) & ((1usize << self.properties.lp) - 1)) << self.properties.lc)
            + (usize::from(previous) >> (8 - self.properties.lc));
        let base = context * LITERAL_BLOCK;
        let mut symbol = 1usize;

        if self.state >= 7 {
            let distance = self.rep[0] + 1;
            if distance > out.len() - scope.dict_base {
                return Err(CodecError::InvalidBackReference {
                    position: BitPosition::new(out.len() as u64, 0),
                    distance: distance as u32,
                    available: (out.len() - scope.dict_base) as u32,
                });
            }
            let mut match_byte = usize::from(out[out.len() - distance]);
            while symbol < 0x100 {
                let match_bit = (match_byte >> 7) & 1;
                match_byte = (match_byte << 1) & 0x1FF;
                let bit = range.decode_bit(&mut self.literal[base + ((1 + match_bit) << 8) + symbol])? as usize;
                symbol = (symbol << 1) | bit;
                if match_bit != bit {
                    break;
                }
            }
        }

        while symbol < 0x100 {
            symbol = (symbol << 1) | range.decode_bit(&mut self.literal[base + symbol])? as usize;
        }
        Ok((symbol - 0x100) as u8)
    }

    /// Decodes a new match distance from its slot, direct, and alignment bits.
    fn decode_distance(&mut self, range: &mut RangeDecoder<'_>, len: usize) -> Result<usize, CodecError> {
        let len_state = if len > 3 { 3 } else { len };
        let slot = range.decode_tree(6, &mut self.pos_slot[len_state])? as usize;
        if slot < 4 {
            return Ok(slot);
        }
        let direct_bits = (slot >> 1) - 1;
        let mut distance = (2 ^ (slot & 1)) << direct_bits;
        if slot < 14 {
            distance += range.decode_tree_reverse(direct_bits as u32, &mut self.spec_pos, distance - slot)? as usize;
        }
        else {
            distance += (range.decode_direct((direct_bits - 4) as u32)? as usize) << 4;
            distance += range.decode_tree_reverse(4, &mut self.align, 0)? as usize;
        }
        Ok(distance)
    }

    /// Copies a match of `len` bytes at back-distance `dist` into the output.
    fn copy_match(&mut self, out: &mut Vec<u8>, len: usize, dist: usize, scope: DecodeScope, limits: &CodecLimits) -> Result<(), CodecError> {
        let available = out.len() - scope.dict_base;
        if dist == 0 || dist > available || dist > scope.window {
            return Err(CodecError::InvalidBackReference {
                position: BitPosition::new(out.len() as u64, 0),
                distance: dist as u32,
                available: available as u32,
            });
        }
        self.reserve(out, len, limits)?;
        out.reserve(len.min(4096));
        for _ in 0..len {
            let byte = out[out.len() - dist];
            out.push(byte);
        }
        Ok(())
    }

    /// Checks the output limit before appending `extra` bytes.
    fn reserve(&self, out: &[u8], extra: usize, limits: &CodecLimits) -> Result<(), CodecError> {
        if (out.len() + extra) as u64 > limits.output_limit() {
            return Err(CodecError::OutputLimitExceeded { produced: out.len() as u64, limit: limits.output_limit() });
        }
        Ok(())
    }
}
