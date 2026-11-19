//! Minimal host executable wrappers per platform (PE / ELF / Mach-O), used to
//! embed native logic and a read-only tail such as an Asgard UI wire segment.

use crate::{err, error::Result};

use data_binary_mach_o::mach_o::MachOImageBuilder;
use data_binary_x64::{native_elf::NativeElfImageBuilder, native_pe::NativeImageBuilder, x86_64::X64Instruction};

/// Writes host executable bytes for the requested OS platform.
pub fn build_host_executable(platform: &str, native_logic: Option<&[u8]>, rodata_tail: &[u8]) -> Result<Vec<u8>> {
    match platform {
        "windows" => build_pe_executable(native_logic, rodata_tail),
        "linux" => build_elf_executable(native_logic, rodata_tail),
        _ => build_macho_executable(native_logic, rodata_tail),
    }
}

fn build_pe_executable(native_logic: Option<&[u8]>, rodata_tail: &[u8]) -> Result<Vec<u8>> {
    let mut builder = NativeImageBuilder::new();
    if let Some(code) = native_logic {
        builder.add_rdata("host_logic", code);
    }
    if !rodata_tail.is_empty() {
        builder.add_rdata("asgard_rodata", rodata_tail);
    }
    builder.push(X64Instruction::Label("_start".into()));
    builder.push(X64Instruction::Ret);
    builder.build_executable("_start").map_err(|error| err!("{error}"))
}

fn build_elf_executable(native_logic: Option<&[u8]>, rodata_tail: &[u8]) -> Result<Vec<u8>> {
    let mut builder = NativeElfImageBuilder::new();
    if let Some(code) = native_logic {
        builder.add_rodata("host_logic", code);
    }
    if !rodata_tail.is_empty() {
        builder.add_rodata("asgard_rodata", rodata_tail);
    }
    builder.push(X64Instruction::Label("_start".into()));
    builder.push(X64Instruction::Ret);
    builder.build_executable("_start").map_err(|error| err!("{error}"))
}

fn build_macho_executable(native_logic: Option<&[u8]>, rodata_tail: &[u8]) -> Result<Vec<u8>> {
    let mut builder = MachOImageBuilder::new();
    if let Some(code) = native_logic {
        builder.set_code(code);
    }
    let mut rodata = Vec::new();
    rodata.extend_from_slice(rodata_tail);
    if !rodata.is_empty() {
        builder.add_rodata(&rodata);
    }
    builder.build_executable().map_err(|error| err!("{error}"))
}
