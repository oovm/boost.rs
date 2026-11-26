//! RFC 7932 meta-block, block and command decoding.

use super::{
    BrotliError,
    bit::BitReader,
    dictionary,
    huffman::Huffman,
    tables::{BLOCK_LEN_PREFIX, CONTEXT_LOOKUP, cmd_lut},
};

/// Number of literal symbols in the literal prefix code.
const NUM_LITERAL_CODES: usize = 256;
/// Number of insert-and-copy symbols in the command prefix code.
const NUM_COMMAND_CODES: usize = 704;
/// Number of symbols in a block length prefix code.
const NUM_BLOCK_LEN_CODES: usize = 26;
/// Order in which the code length code lengths are transmitted.
const CODE_LENGTH_ORDER: [usize; 18] = [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];
/// Number of short distance codes that refer to the recent distance ring.
const NUM_DISTANCE_SHORT_CODES: u32 = 16;
/// Upper bound on a distance accepted by the decoder.
const MAX_ALLOWED_DISTANCE: u32 = 0x7fff_fffc;
/// Number of literals context bits (64 contexts per literal block type).
const LITERAL_CONTEXT_BITS: usize = 6;
/// Number of distance context bits (4 contexts per distance block type).
const DISTANCE_CONTEXT_BITS: usize = 2;

/// Short distance codes: `(slots_back, delta)` relative to the distance ring.
///
/// Code 0 (the previous distance) is handled separately because it does not
/// advance the ring.
const SHORT_DISTANCE_CODES: [(u32, i32); 16] =
    [(0, 0), (2, 0), (3, 0), (0, 0), (1, -1), (1, 1), (1, -2), (1, 2), (1, -3), (1, 3), (2, -1), (2, 1), (2, -2), (2, 2), (2, -3), (2, 3)];

/// Block type and block length state for one of the three block categories.
struct BlockState {
    /// Number of block types, at least one.
    num_types: usize,
    /// Prefix code over block type symbols, present when `num_types >= 2`.
    type_tree: Option<Huffman>,
    /// Prefix code over block length symbols, present when `num_types >= 2`.
    length_tree: Option<Huffman>,
    /// Ring of the two most recent block types.
    recent: [u32; 2],
    /// Number of symbols left in the current block.
    remaining: u32,
}

impl BlockState {
    /// Creates the initial state: block type 0, no switching.
    fn new() -> Self {
        Self { num_types: 1, type_tree: None, length_tree: None, recent: [1, 0], remaining: 1 << 24 }
    }

    /// Type of the block that is currently being decoded.
    fn current(&self) -> usize {
        self.recent[1] as usize
    }

    /// Reads a block switch command and updates the current block.
    fn switch(&mut self, reader: &mut BitReader<'_>) -> Result<(), BrotliError> {
        let type_tree = self.type_tree.as_ref().ok_or(BrotliError::Corrupt("missing block type code"))?;
        let length_tree = self.length_tree.as_ref().ok_or(BrotliError::Corrupt("missing block length code"))?;
        let code = u32::from(type_tree.decode(reader)?);
        self.remaining = read_block_length(reader, length_tree)?;
        let num_types = self.num_types as u32;
        let mut next = match code {
            0 => self.recent[0],
            1 => self.recent[1] + 1,
            code => code - 2,
        };
        if next >= num_types {
            next -= num_types;
        }
        self.recent[0] = self.recent[1];
        self.recent[1] = next;
        Ok(())
    }

    /// Consumes one symbol from the current block.
    fn consume(&mut self, reader: &mut BitReader<'_>) -> Result<usize, BrotliError> {
        if self.remaining == 0 {
            self.switch(reader)?;
        }
        self.remaining -= 1;
        Ok(self.current())
    }
}

/// One decode: the bytes produced, the input consumed, and why it stopped.
pub(crate) struct Decoded {
    /// Bytes produced before the stream completed or stopped.
    pub(crate) output: Vec<u8>,
    /// Input bytes consumed by the decoder.
    pub(crate) consumed: usize,
    /// Terminal error, or `None` when the final meta-block decoded.
    pub(crate) error: Option<BrotliError>,
}

/// Decodes one complete Brotli stream, keeping the prefix produced before a stop.
pub(crate) fn decompress(input: &[u8], limit: usize) -> Decoded {
    let mut reader = BitReader::new(input);
    let mut output = Vec::new();
    let error = decode_stream(&mut reader, &mut output, limit).err();
    let consumed = reader.consumed_bytes();
    Decoded { output, consumed, error }
}

/// Decodes meta-blocks into `output` until the final one or an error.
fn decode_stream(reader: &mut BitReader<'_>, output: &mut Vec<u8>, limit: usize) -> Result<(), BrotliError> {
    let window_bits = read_window_bits(reader)?;
    let max_backward = (1usize << window_bits) - 16;
    // The distance cache is shared by every meta-block of the stream.
    let mut distance_ring = [16u32, 15, 11, 4];
    let mut distance_ring_index: i64 = 0;

    loop {
        let is_last = reader.read_bits(1)? != 0;
        if is_last && reader.read_bits(1)? != 0 {
            break;
        }

        let nibbles_code = reader.read_bits(2)? as usize;
        if nibbles_code == 3 {
            // Metadata meta-block: skipped and never produces output.
            if reader.read_bits(1)? != 0 {
                return Err(BrotliError::Corrupt("metadata reserved bit set"));
            }
            let skip_bytes = reader.read_bits(2)?;
            let skip_length = if skip_bytes == 0 { 0 } else { reader.read_bits(skip_bytes * 8)? as usize + 1 };
            reader.align_to_byte();
            for _ in 0..skip_length {
                reader.read_aligned_byte()?;
            }
            if is_last {
                break;
            }
            continue;
        }

        let nibbles = nibbles_code + 4;
        let mut meta_block_length: u64 = 0;
        for nibble in 0..nibbles {
            let value = u64::from(reader.read_bits(4)?);
            if nibble + 1 == nibbles && nibbles > 4 && value == 0 {
                return Err(BrotliError::Corrupt("non-canonical meta-block length"));
            }
            meta_block_length |= value << (nibble * 4);
        }
        let meta_block_length = meta_block_length + 1;
        let is_uncompressed = !is_last && reader.read_bits(1)? != 0;
        let meta_block_length = usize::try_from(meta_block_length).map_err(|_| BrotliError::Corrupt("meta-block length overflow"))?;

        if is_uncompressed {
            reader.align_to_byte();
            if output.len().saturating_add(meta_block_length) > limit {
                return Err(BrotliError::LimitExceeded);
            }
            for _ in 0..meta_block_length {
                let byte = reader.read_aligned_byte()?;
                output.push(byte);
            }
        }
        else {
            decode_compressed_meta_block(reader, meta_block_length, max_backward, &mut distance_ring, &mut distance_ring_index, output, limit)?;
        }

        if is_last {
            break;
        }
    }

    Ok(())
}

/// Decodes WBITS, the encoded window size (RFC 7932 section 9.1).
fn read_window_bits(reader: &mut BitReader<'_>) -> Result<u32, BrotliError> {
    let window_bits = if reader.read_bits(1)? == 0 {
        16
    }
    else {
        let value = reader.read_bits(3)?;
        if value != 0 {
            17 + value
        }
        else {
            let value = reader.read_bits(3)?;
            if value != 0 { 8 + value } else { 17 }
        }
    };
    if !(10..=24).contains(&window_bits) {
        return Err(BrotliError::Corrupt("invalid window size"));
    }
    Ok(window_bits)
}

/// Decodes the fixed variable-length code used for counts (RFC 7932 section 9.1).
fn read_varlen_uint8(reader: &mut BitReader<'_>) -> Result<u32, BrotliError> {
    if reader.read_bits(1)? == 0 {
        return Ok(0);
    }
    let nibble = reader.read_bits(3)?;
    if nibble == 0 { Ok(1) } else { Ok((1 << nibble) + reader.read_bits(nibble)?) }
}

/// Reads one block length.
fn read_block_length(reader: &mut BitReader<'_>, tree: &Huffman) -> Result<u32, BrotliError> {
    let code = usize::from(tree.decode(reader)?);
    let (base, extra_bits) = *BLOCK_LEN_PREFIX.get(code).ok_or(BrotliError::Corrupt("block length code out of range"))?;
    Ok(u32::from(base) + reader.read_bits(u32::from(extra_bits))?)
}

/// Decodes the number of Huffman trees and their context map.
fn read_context_map(reader: &mut BitReader<'_>, size: usize) -> Result<(usize, Vec<u8>), BrotliError> {
    let num_trees = read_varlen_uint8(reader)? as usize + 1;
    let mut map = vec![0u8; size];
    if num_trees <= 1 {
        return Ok((num_trees, map));
    }
    let use_rle = reader.read_bits(1)? != 0;
    let max_run = if use_rle { reader.read_bits(4)? as usize + 1 } else { 0 };
    let tree = read_prefix_code(reader, num_trees + max_run)?;
    let mut index = 0;
    while index < size {
        let code = usize::from(tree.decode(reader)?);
        if code == 0 {
            index += 1;
        }
        else if code > max_run {
            let value = code - max_run;
            if value > u8::MAX as usize {
                return Err(BrotliError::Corrupt("context id out of range"));
            }
            map[index] = value as u8;
            index += 1;
        }
        else {
            let run = (1usize << code) + reader.read_bits(code as u32)? as usize;
            if index + run > size {
                return Err(BrotliError::Corrupt("context map run exceeds map size"));
            }
            index += run;
        }
    }
    if reader.read_bits(1)? != 0 {
        inverse_move_to_front(&mut map);
    }
    Ok((num_trees, map))
}

/// Applies the inverse move-to-front transform in place.
fn inverse_move_to_front(values: &mut [u8]) {
    let mut table = [0u8; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        *slot = index as u8;
    }
    for value in values.iter_mut() {
        let index = usize::from(*value);
        let moved = table[index];
        if index != 0 {
            table.copy_within(0..index, 1);
            table[0] = moved;
        }
        *value = moved;
    }
}

/// Reads one prefix code (Huffman tree) for an alphabet of `alphabet_size`.
fn read_prefix_code(reader: &mut BitReader<'_>, alphabet_size: usize) -> Result<Huffman, BrotliError> {
    if alphabet_size < 2 {
        return Err(BrotliError::Corrupt("prefix code alphabet is too small"));
    }
    let selector = reader.read_bits(2)? as usize;
    if selector == 1 { read_simple_prefix_code(reader, alphabet_size) } else { read_complex_prefix_code(reader, alphabet_size, selector) }
}

/// Number of bits needed to represent `value` (at least one for non-zero input).
fn bit_length(value: u32) -> u32 {
    32 - value.leading_zeros()
}

/// Reads the "simple" prefix code form, which lists 1..=4 symbols directly.
fn read_simple_prefix_code(reader: &mut BitReader<'_>, alphabet_size: usize) -> Result<Huffman, BrotliError> {
    let count_code = reader.read_bits(2)? as usize;
    let symbol_bits = bit_length((alphabet_size - 1) as u32);
    let count = if count_code == 3 { 4 } else { count_code + 1 };
    let mut symbols = [0u16; 4];
    for slot in symbols.iter_mut().take(count) {
        let value = reader.read_bits(symbol_bits)? as usize;
        if value >= alphabet_size {
            return Err(BrotliError::Corrupt("simple prefix symbol out of range"));
        }
        *slot = value as u16;
    }
    for left in 0..count {
        for right in left + 1..count {
            if symbols[left] == symbols[right] {
                return Err(BrotliError::Corrupt("duplicate simple prefix symbol"));
            }
        }
    }
    if count == 1 {
        return Ok(Huffman::single(symbols[0]));
    }
    // Four symbols have two possible code shapes, selected by one extra bit.
    let tree_select = if count_code == 3 { reader.read_bits(1)? as usize } else { 0 };
    let mut lengths = vec![0u8; alphabet_size];
    match count {
        2 => {
            lengths[symbols[0] as usize] = 1;
            lengths[symbols[1] as usize] = 1;
        }
        3 => {
            lengths[symbols[0] as usize] = 1;
            lengths[symbols[1] as usize] = 2;
            lengths[symbols[2] as usize] = 2;
        }
        _ => {
            if tree_select == 0 {
                for symbol in symbols {
                    lengths[symbol as usize] = 2;
                }
            }
            else {
                lengths[symbols[0] as usize] = 1;
                lengths[symbols[1] as usize] = 2;
                lengths[symbols[2] as usize] = 3;
                lengths[symbols[3] as usize] = 3;
            }
        }
    }
    Huffman::from_lengths(&lengths, alphabet_size)
}

/// Decodes one code length of the code length code (RFC 7932 section 3.5).
fn read_code_length_code(reader: &mut BitReader<'_>) -> Result<u8, BrotliError> {
    let first = reader.read_bits(1)?;
    let second = reader.read_bits(1)?;
    match (first, second) {
        (0, 0) => Ok(0),
        (1, 0) => Ok(4),
        (0, 1) => Ok(3),
        _ => {
            if reader.read_bits(1)? == 0 {
                Ok(2)
            }
            else if reader.read_bits(1)? == 0 {
                Ok(1)
            }
            else {
                Ok(5)
            }
        }
    }
}

/// Reads the "complex" prefix code form: a two-level code length encoding.
fn read_complex_prefix_code(reader: &mut BitReader<'_>, alphabet_size: usize, skip: usize) -> Result<Huffman, BrotliError> {
    let mut code_length_lengths = [0u8; 18];
    let mut num_codes = 0i32;
    let mut code_length_space = 32i32;
    for &symbol in CODE_LENGTH_ORDER.iter().skip(skip) {
        let value = read_code_length_code(reader)?;
        code_length_lengths[symbol] = value;
        if value != 0 {
            code_length_space -= 32 >> value;
            num_codes += 1;
            if code_length_space <= 0 {
                break;
            }
        }
    }
    if !(num_codes == 1 || code_length_space == 0) {
        return Err(BrotliError::Corrupt("invalid code length code"));
    }
    let length_tree = Huffman::from_lengths(&code_length_lengths, 18)?;

    let mut lengths = vec![0u8; alphabet_size];
    let mut symbol = 0usize;
    let mut space: i64 = 32768;
    let mut previous_length: u32 = 8;
    let mut repeat = 0u32;
    let mut repeat_length = 0u32;
    while symbol < alphabet_size && space > 0 {
        let code = u32::from(length_tree.decode(reader)?);
        if code < 16 {
            repeat = 0;
            if code != 0 {
                lengths[symbol] = code as u8;
                previous_length = code;
                space -= 32768 >> code;
            }
            symbol += 1;
        }
        else {
            let extra_bits = if code == 16 { 2 } else { 3 };
            let delta_bits = reader.read_bits(extra_bits)?;
            let new_length = if code == 16 { previous_length } else { 0 };
            if repeat_length != new_length {
                repeat = 0;
                repeat_length = new_length;
            }
            let old_repeat = repeat;
            if repeat > 0 {
                repeat -= 2;
                repeat <<= extra_bits;
            }
            repeat += delta_bits + 3;
            let delta = (repeat - old_repeat) as usize;
            if symbol + delta > alphabet_size {
                return Err(BrotliError::Corrupt("code length repeat overflow"));
            }
            if repeat_length != 0 {
                let value = repeat_length as u8;
                for _ in 0..delta {
                    lengths[symbol] = value;
                    symbol += 1;
                }
                space -= (delta as i64) << (15 - repeat_length);
            }
            else {
                symbol += delta;
            }
        }
        if space < 0 {
            return Err(BrotliError::Corrupt("over-subscribed prefix code"));
        }
    }
    if space != 0 {
        return Err(BrotliError::Corrupt("incomplete prefix code"));
    }
    Huffman::from_lengths(&lengths, alphabet_size)
}

/// Reads a group of `count` prefix codes over the same alphabet.
fn read_tree_group(reader: &mut BitReader<'_>, count: usize, alphabet_size: usize) -> Result<Vec<Huffman>, BrotliError> {
    let mut trees = Vec::with_capacity(count);
    for _ in 0..count {
        trees.push(read_prefix_code(reader, alphabet_size)?);
    }
    Ok(trees)
}

/// Decodes the body of a compressed meta-block of exactly `remaining` bytes.
fn decode_compressed_meta_block(
    reader: &mut BitReader<'_>,
    meta_block_length: usize,
    max_backward: usize,
    distance_ring: &mut [u32; 4],
    distance_ring_index: &mut i64,
    output: &mut Vec<u8>,
    limit: usize,
) -> Result<(), BrotliError> {
    let mut blocks: [BlockState; 3] = [BlockState::new(), BlockState::new(), BlockState::new()];
    for block in blocks.iter_mut() {
        block.num_types = read_varlen_uint8(reader)? as usize + 1;
        if block.num_types >= 2 {
            let type_tree = read_prefix_code(reader, block.num_types + 2)?;
            let length_tree = read_prefix_code(reader, NUM_BLOCK_LEN_CODES)?;
            block.remaining = read_block_length(reader, &length_tree)?;
            block.type_tree = Some(type_tree);
            block.length_tree = Some(length_tree);
        }
    }
    let num_literal_types = blocks[0].num_types;
    let num_command_types = blocks[1].num_types;
    let num_distance_types = blocks[2].num_types;

    let postfix_bits = reader.read_bits(2)?;
    let direct_codes = (reader.read_bits(4)? as usize) << postfix_bits;
    let distance_alphabet = NUM_DISTANCE_SHORT_CODES as usize + direct_codes + (48usize << postfix_bits);

    let mut context_modes = vec![0u8; num_literal_types];
    for mode in context_modes.iter_mut() {
        *mode = reader.read_bits(2)? as u8;
    }

    let (num_literal_trees, literal_map) = read_context_map(reader, num_literal_types * (1 << LITERAL_CONTEXT_BITS))?;
    let (num_distance_trees, distance_map) = read_context_map(reader, num_distance_types * (1 << DISTANCE_CONTEXT_BITS))?;

    let literal_trees = read_tree_group(reader, num_literal_trees, NUM_LITERAL_CODES)?;
    let command_trees = read_tree_group(reader, num_command_types, NUM_COMMAND_CODES)?;
    let distance_trees = read_tree_group(reader, num_distance_trees, distance_alphabet)?;

    let mut remaining = meta_block_length;

    while remaining > 0 {
        let command_type = blocks[1].consume(reader)?;
        let command_tree = command_trees.get(command_type).ok_or(BrotliError::Corrupt("command tree out of range"))?;
        let command_index = usize::from(command_tree.decode(reader)?);
        if command_index >= NUM_COMMAND_CODES {
            return Err(BrotliError::Corrupt("command code out of range"));
        }
        let command = *cmd_lut(command_index);

        let insert_length = usize::from(command.insert_offset) + reader.read_bits(u32::from(command.insert_extra_bits))? as usize;
        let copy_length = usize::from(command.copy_offset) + reader.read_bits(u32::from(command.copy_extra_bits))? as usize;

        if insert_length > remaining {
            return Err(BrotliError::Corrupt("insert length exceeds meta-block"));
        }
        remaining -= insert_length;

        for _ in 0..insert_length {
            let block_type = blocks[0].consume(reader)?;
            let mode = usize::from(context_modes[block_type] & 3);
            let previous = output.last().copied().unwrap_or(0) as usize;
            let second_previous = if output.len() >= 2 { output[output.len() - 2] as usize } else { 0 };
            let context = (usize::from(CONTEXT_LOOKUP[mode][previous]) | usize::from(CONTEXT_LOOKUP[mode][256 + second_previous]))
                & ((1 << LITERAL_CONTEXT_BITS) - 1);
            let tree_index = literal_map[block_type * (1 << LITERAL_CONTEXT_BITS) + context] as usize;
            let tree = literal_trees.get(tree_index).ok_or(BrotliError::Corrupt("literal tree out of range"))?;
            let literal = tree.decode(reader)? as u8;
            if output.len() >= limit {
                return Err(BrotliError::LimitExceeded);
            }
            output.push(literal);
        }

        // If the inserted literals complete the meta-block there is no distance
        // or copy for this command.
        if remaining == 0 {
            break;
        }

        // Distance: either implied by the command (always "previous distance")
        // or read from the distance prefix code.
        let (distance, distance_context) = if command.distance_code >= 0 {
            *distance_ring_index -= 1;
            (distance_ring[(*distance_ring_index & 3) as usize], 1i64)
        }
        else {
            let block_type = blocks[2].consume(reader)?;
            let tree_index = distance_map[block_type * (1 << DISTANCE_CONTEXT_BITS) + usize::from(command.context & 3)] as usize;
            let tree = distance_trees.get(tree_index).ok_or(BrotliError::Corrupt("distance tree out of range"))?;
            let symbol = u32::from(tree.decode(reader)?);
            if symbol < NUM_DISTANCE_SHORT_CODES {
                (take_short_distance(symbol, distance_ring_index, distance_ring)?, 1i64)
            }
            else {
                (explicit_distance(symbol, postfix_bits as u32, direct_codes, reader)?, 0i64)
            }
        };

        // The maximum backward distance is limited by what has been decoded so
        // far; anything beyond it is a static dictionary reference.
        let max_distance = output.len().min(max_backward) as u32;
        if distance > max_distance {
            // Static dictionary reference: the distance points past the window.
            if distance > MAX_ALLOWED_DISTANCE {
                return Err(BrotliError::Corrupt("distance is too large"));
            }
            let word_id = distance - max_distance - 1;
            let transformed = dictionary::word(copy_length, word_id)?;
            if output.len().saturating_add(transformed.len()) > limit {
                return Err(BrotliError::LimitExceeded);
            }
            if transformed.len() > remaining {
                return Err(BrotliError::Corrupt("dictionary word exceeds meta-block"));
            }
            remaining -= transformed.len();
            *distance_ring_index += distance_context;
            output.extend_from_slice(&transformed);
        }
        else {
            let distance = distance as usize;
            if distance == 0 || distance > output.len() {
                return Err(BrotliError::Corrupt("invalid backward reference"));
            }
            if copy_length > remaining {
                return Err(BrotliError::Corrupt("copy length exceeds meta-block"));
            }
            if output.len().saturating_add(copy_length) > limit {
                return Err(BrotliError::LimitExceeded);
            }
            remaining -= copy_length;
            distance_ring[(*distance_ring_index & 3) as usize] = distance as u32;
            *distance_ring_index += 1;
            let start = output.len() - distance;
            for offset in 0..copy_length {
                let byte = output[start + offset];
                output.push(byte);
            }
        }
    }

    Ok(())
}

/// Resolves a short distance code (0..=15) against the distance ring.
fn take_short_distance(code: u32, ring_index: &mut i64, ring: &[u32; 4]) -> Result<u32, BrotliError> {
    if code == 0 {
        *ring_index -= 1;
        return Ok(ring[(*ring_index & 3) as usize]);
    }
    let (slots_back, delta) = SHORT_DISTANCE_CODES[code as usize];
    let slot = ((*ring_index - i64::from(slots_back)) & 3) as usize;
    let distance = i64::from(ring[slot]) + i64::from(delta);
    if distance <= 0 {
        return Err(BrotliError::Corrupt("invalid short distance"));
    }
    Ok(distance as u32)
}

/// Converts an explicit distance symbol into a backward distance.
fn explicit_distance(symbol: u32, postfix_bits: u32, direct_codes: usize, reader: &mut BitReader<'_>) -> Result<u32, BrotliError> {
    let num_direct = i64::from(NUM_DISTANCE_SHORT_CODES) + direct_codes as i64;
    let distance_code = i64::from(symbol) - num_direct;
    let value = if distance_code >= 0 {
        let postfix_mask = (1i64 << postfix_bits) - 1;
        let postfix = distance_code & postfix_mask;
        let shifted = distance_code >> postfix_bits;
        let extra_bits = (shifted >> 1) as u32 + 1;
        let bits = i64::from(reader.read_bits(extra_bits)?);
        let offset = (((shifted & 1) + 2) << extra_bits) - 4;
        ((offset + bits) << postfix_bits) + postfix + num_direct
    }
    else {
        i64::from(symbol)
    };
    let distance = value - i64::from(NUM_DISTANCE_SHORT_CODES) + 1;
    if distance <= 0 || distance > i64::from(MAX_ALLOWED_DISTANCE) {
        return Err(BrotliError::Corrupt("invalid distance"));
    }
    Ok(distance as u32)
}
