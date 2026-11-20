//! Portable candidate decoding of raw archive entry names.
//!
//! Container codecs keep entry names as undecoded bytes because writers
//! disagree about the declared encoding. This module owns the portable
//! mechanism: strict UTF-8 first, then each caller-supplied regional decoder
//! in order, then a format-defined terminal. Regional policy (for example a
//! GBK fallback for one locale) stays with the caller as a [`RegionalDecoder`].

use std::fmt;

/// A caller-supplied regional name decoder.
///
/// It returns `None` when the bytes are not valid in that encoding, so the
/// ladder continues with the next candidate.
pub type RegionalDecoder = &'static (dyn Fn(&[u8]) -> Option<String> + Sync);

/// Terminal decoding applied when UTF-8 and every regional decoder fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameTerminal {
    /// CP437, the legacy default of the ZIP specification. Never fails.
    Cp437,
    /// Replacement-character decoding, the portable terminal for byte strings
    /// with no legacy code page defined, such as tar paths. Never fails.
    LossyUtf8,
}

/// Decodes raw entry name bytes through the portable candidate ladder.
///
/// Order: strict UTF-8, then each `regional` decoder in the given order, then
/// `terminal`. An empty input decodes to the empty string.
pub fn decode_entry_name(raw: &[u8], regional: &[RegionalDecoder], terminal: NameTerminal) -> String {
    if let Ok(text) = std::str::from_utf8(raw) {
        return text.to_string();
    }
    for decoder in regional {
        if let Some(text) = decoder(raw) {
            return text;
        }
    }
    match terminal {
        NameTerminal::Cp437 => decode_cp437(raw),
        NameTerminal::LossyUtf8 => String::from_utf8_lossy(raw).into_owned(),
    }
}

/// CP437 mapping for bytes `0x80..=0xFF`.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢',
    '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖',
    '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙',
    '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩',
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// Decodes bytes as CP437, the legacy ZIP default code page.
///
/// Every byte maps, so this never fails and never reports an error.
pub fn decode_cp437(raw: &[u8]) -> String {
    let mut text = String::with_capacity(raw.len());
    for &byte in raw {
        if byte < 0x80 {
            text.push(char::from(byte));
        }
        else {
            text.push(CP437_HIGH[usize::from(byte - 0x80)]);
        }
    }
    text
}

impl fmt::Display for NameTerminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cp437 => write!(f, "cp437 terminal"),
            Self::LossyUtf8 => write!(f, "lossy utf-8 terminal"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake "regional" encoding that only accepts the single byte `0xFF`.
    fn only_ff(raw: &[u8]) -> Option<String> {
        if raw == [0xFF] { Some("FF".to_string()) } else { None }
    }

    const ONLY_FF: RegionalDecoder = &only_ff;

    #[test]
    fn valid_utf8_wins_regardless_of_terminal() {
        assert_eq!(decode_entry_name("文档/报告.txt".as_bytes(), &[ONLY_FF], NameTerminal::Cp437), "文档/报告.txt");
        assert_eq!(decode_entry_name(b"plain.txt", &[], NameTerminal::LossyUtf8), "plain.txt");
    }

    #[test]
    fn regional_decoders_run_before_the_terminal() {
        assert_eq!(decode_entry_name(&[0xFF], &[ONLY_FF], NameTerminal::Cp437), "FF");
        assert_eq!(decode_entry_name(&[0xFF], &[], NameTerminal::Cp437), "\u{a0}");
    }

    #[test]
    fn regional_order_is_respected() {
        fn first(raw: &[u8]) -> Option<String> {
            if raw.len() == 1 { Some("first".to_string()) } else { None }
        }
        fn second(raw: &[u8]) -> Option<String> {
            if raw.len() == 1 { Some("second".to_string()) } else { None }
        }
        const FIRST: RegionalDecoder = &first;
        const SECOND: RegionalDecoder = &second;
        assert_eq!(decode_entry_name(&[0x80], &[FIRST, SECOND], NameTerminal::Cp437), "first");
        assert_eq!(decode_entry_name(&[0x80], &[SECOND, FIRST], NameTerminal::Cp437), "second");
    }

    #[test]
    fn lossy_terminal_never_fails() {
        assert_eq!(decode_entry_name(&[0x65, 0x64, 0x82, 0x69], &[], NameTerminal::LossyUtf8), "ed\u{fffd}i");
    }

    #[test]
    fn cp437_maps_the_full_high_range() {
        // 0x82 is `é`, 0x9B is `¢`, 0xFF is a no-break space.
        assert_eq!(decode_cp437(&[0x82]), "é");
        assert_eq!(decode_cp437(&[0x9B]), "¢");
        assert_eq!(decode_cp437(&[0xFF]), "\u{a0}");
        assert_eq!(decode_cp437(b"abc"), "abc");
    }

    #[test]
    fn empty_names_decode_to_empty() {
        assert_eq!(decode_entry_name(&[], &[ONLY_FF], NameTerminal::Cp437), "");
    }
}
