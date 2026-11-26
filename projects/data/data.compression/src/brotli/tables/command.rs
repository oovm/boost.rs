//! The 704-entry insert-and-copy command lookup table.

mod cmd_lut_0;
mod cmd_lut_1;
mod cmd_lut_2;
mod cmd_lut_3;

/// One entry of the insert-and-copy command lookup table.
#[derive(Clone, Copy)]
pub(crate) struct CmdLutEntry {
    pub(crate) insert_extra_bits: u8,
    pub(crate) copy_extra_bits: u8,
    pub(crate) distance_code: i8,
    pub(crate) context: u8,
    pub(crate) insert_offset: u16,
    pub(crate) copy_offset: u16,
}

/// Number of lookup entries stored in each chunk.
const CHUNK: usize = 176;

/// Returns the insert-and-copy command entry at `index`.
///
/// Callers must keep `index` below the 704 commands defined by RFC 7932.
pub(crate) fn cmd_lut(index: usize) -> &'static CmdLutEntry {
    let chunk = match index / CHUNK {
        0 => &cmd_lut_0::CMD_LUT_0,
        1 => &cmd_lut_1::CMD_LUT_1,
        2 => &cmd_lut_2::CMD_LUT_2,
        _ => &cmd_lut_3::CMD_LUT_3,
    };
    &chunk[index % CHUNK]
}
