//! 双字节 `0F` opcode map。

use crate::decode::{
    Decoded,
    operand::{Operand, Reg, RegKind},
    prefix::Reader,
};

use super::{
    Ctx, imm_operand, jcc_name,
    map38::three_byte,
    modrm, reg_rm, rm_operand, rm_reg,
    sse::{
        compare_float, cvt_float_to_int, cvt_int_to_float, group9, group15, move_special, packed, packed_late, shift_double, shift_group,
        shuffle, sse_arith, system_ptr,
    },
};

/// SSE 或 AVX 指令的强制前缀。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mandatory {
    /// 无强制前缀。
    None,
    /// `0x66` 操作数宽度前缀。
    P66,
    /// `0xF3` 重复前缀。
    F3,
    /// `0xF2` 重复前缀。
    F2,
}

/// 从已解码前缀中提取强制前缀。
pub(super) fn mandatory(ctx: &Ctx) -> Mandatory {
    match ctx.p.rep {
        Some(0xF3) => Mandatory::F3,
        Some(0xF2) => Mandatory::F2,
        _ => {
            if ctx.p.opsize {
                Mandatory::P66
            }
            else {
                Mandatory::None
            }
        }
    }
}

/// 构造向量寄存器操作数。
pub(super) fn vec_reg(kind: RegKind, num: u8) -> Operand {
    Operand::Reg(Reg::vec(kind, num))
}

/// 解码 `reg, rm` 向量指令。
pub(super) fn vec_ge(r: &mut Reader<'_>, ctx: &Ctx, kind: RegKind, bits: u32, name: &'static str) -> Result<Decoded, ()> {
    let (m, mem) = modrm(r, ctx, bits)?;
    let reg = vec_reg(kind, ctx.reg_num(&m));
    let rm = match mem {
        Some(mem) => Operand::Mem(mem),
        None => vec_reg(kind, ctx.rm_num(&m)),
    };
    Ok(Decoded::new(name, vec![reg, rm]))
}

/// 解码 `rm, reg` 向量指令。
pub(super) fn vec_eg(r: &mut Reader<'_>, ctx: &Ctx, kind: RegKind, bits: u32, name: &'static str) -> Result<Decoded, ()> {
    let (m, mem) = modrm(r, ctx, bits)?;
    let reg = vec_reg(kind, ctx.reg_num(&m));
    let rm = match mem {
        Some(mem) => Operand::Mem(mem),
        None => vec_reg(kind, ctx.rm_num(&m)),
    };
    Ok(Decoded::new(name, vec![rm, reg]))
}

/// 解码 `reg, rm, imm8` 向量指令。
pub(super) fn vec_ge_imm(r: &mut Reader<'_>, ctx: &Ctx, kind: RegKind, bits: u32, name: &'static str) -> Result<Decoded, ()> {
    let (m, mem) = modrm(r, ctx, bits)?;
    let reg = vec_reg(kind, ctx.reg_num(&m));
    let rm = match mem {
        Some(mem) => Operand::Mem(mem),
        None => vec_reg(kind, ctx.rm_num(&m)),
    };
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![reg, rm, imm]))
}

/// 解码双字节 `0F` map 中的 opcode。
pub(super) fn decode_0f(r: &mut Reader<'_>, ctx: Ctx) -> Result<Decoded, ()> {
    let op = r.byte().ok_or(())?;
    let mp = mandatory(&ctx);
    let size = ctx.op_size;
    match op {
        0x05 => Ok(Decoded::bare("syscall")),
        0x06 => Ok(Decoded::bare("clts")),
        0x07 => Ok(Decoded::bare("sysret")),
        0x08 => Ok(Decoded::bare("invd")),
        0x09 => Ok(Decoded::bare("wbinvd")),
        0x0B => Ok(Decoded::bare("ud2")),
        0x0D => {
            let (m, mem) = modrm(r, &ctx, size)?;
            let rm = rm_operand(&ctx, &m, mem, size);
            Ok(Decoded::new("prefetchw", vec![rm]))
        }
        0x10..=0x17 => mov_vector(r, &ctx, op, mp),
        0x18 => prefetch(r, &ctx),
        0x1E => endbr(r, &ctx),
        0x1F => {
            let (m, mem) = modrm(r, &ctx, size)?;
            let rm = rm_operand(&ctx, &m, mem, size);
            Ok(Decoded::new("nop", vec![rm]))
        }
        0x20..=0x23 => mov_control(r, &ctx, op),
        0x28 | 0x29 => mov_aligned(r, &ctx, op, mp),
        0x2A => cvt_int_to_float(r, &ctx, mp),
        0x2B => {
            let name = match mp {
                Mandatory::None => "movntps",
                Mandatory::P66 => "movntpd",
                _ => return Err(()),
            };
            vec_eg(r, &ctx, RegKind::Xmm, 128, name)
        }
        0x2C | 0x2D => cvt_float_to_int(r, &ctx, op, mp),
        0x2E | 0x2F => compare_float(r, &ctx, op, mp),
        0x30 => Ok(Decoded::bare("wrmsr")),
        0x31 => Ok(Decoded::bare("rdtsc")),
        0x32 => Ok(Decoded::bare("rdmsr")),
        0x33 => Ok(Decoded::bare("rdpmc")),
        0x34 => Ok(Decoded::bare("sysenter")),
        0x35 => Ok(Decoded::bare("sysexit")),
        0x37 => Ok(Decoded::bare("getsec")),
        0x38 => three_byte(r, &ctx, 0x38),
        0x3A => three_byte(r, &ctx, 0x3A),
        0x40..=0x4F => {
            let name = cmov_name(op & 0xF);
            let (reg, rm) = reg_rm(r, &ctx, size)?;
            Ok(Decoded::new(name, vec![reg, rm]))
        }
        0x50..=0x5F => sse_arith(r, &ctx, op, mp),
        0x60..=0x6F => packed(r, &ctx, op, mp),
        0x70 => shuffle(r, &ctx, mp),
        0x71..=0x73 => shift_group(r, &ctx, op, mp),
        0x74..=0x76 => {
            let kind = packed_kind(mp)?;
            let bits = if kind == RegKind::Mm { 64 } else { 128 };
            let name = match op {
                0x74 => "pcmpeqb",
                0x75 => "pcmpeqw",
                _ => "pcmpeqd",
            };
            vec_ge(r, &ctx, kind, bits, name)
        }
        0x77 => Ok(Decoded::bare("emms")),
        0x7E | 0x7F => move_special(r, &ctx, op, mp),
        0x80..=0x8F => {
            let name = jcc_name(op & 0xF);
            let disp_bits = if ctx.bitness == 64 {
                32
            }
            else if size == 16 {
                16
            }
            else {
                32
            };
            let disp = if disp_bits == 16 { i64::from(r.u16().ok_or(())? as i16) } else { i64::from(r.u32().ok_or(())? as i32) };
            let target = ctx.rel(r, disp);
            Ok(Decoded::new(name, vec![Operand::Rel(target)]))
        }
        0x90..=0x9F => {
            let (m, mem) = modrm(r, &ctx, 8)?;
            let rm = rm_operand(&ctx, &m, mem, 8);
            let name = setcc_name(op & 0xF);
            Ok(Decoded::new(name, vec![rm]))
        }
        0xA0 | 0xA1 => {
            let name = if op == 0xA0 { "push" } else { "pop" };
            Ok(Decoded::new(name, vec![Operand::Reg(Reg { kind: RegKind::Seg, size: 16, num: 4, rex: false })]))
        }
        0xA2 => Ok(Decoded::bare("cpuid")),
        0xA3 | 0xAB | 0xB3 | 0xBB => {
            let name = match op {
                0xA3 => "bt",
                0xAB => "bts",
                0xB3 => "btr",
                _ => "btc",
            };
            let (rm, reg) = rm_reg(r, &ctx, size)?;
            Ok(Decoded::new(name, vec![rm, reg]))
        }
        0xA4 | 0xA5 | 0xAC | 0xAD => shift_double(r, &ctx, op),
        0xA8 | 0xA9 => {
            let name = if op == 0xA8 { "push" } else { "pop" };
            Ok(Decoded::new(name, vec![Operand::Reg(Reg { kind: RegKind::Seg, size: 16, num: 5, rex: false })]))
        }
        0xAA => Ok(Decoded::bare("rsm")),
        0xAE => group15(r, &ctx),
        0xAF => {
            let (reg, rm) = reg_rm(r, &ctx, size)?;
            Ok(Decoded::new("imul", vec![reg, rm]))
        }
        0xB0 | 0xB1 => {
            let size = if op == 0xB0 { 8 } else { size };
            let (rm, reg) = rm_reg(r, &ctx, size)?;
            Ok(Decoded::new("cmpxchg", vec![rm, reg]))
        }
        0xB2 => system_ptr(r, &ctx, "lss"),
        0xB4 => system_ptr(r, &ctx, "lfs"),
        0xB5 => system_ptr(r, &ctx, "lgs"),
        0xB6 | 0xB7 | 0xBE | 0xBF => {
            let name = if op == 0xB6 || op == 0xB7 { "movzx" } else { "movsx" };
            let src_bits = if op == 0xB6 || op == 0xBE { 8 } else { 16 };
            let (m, mem) = modrm(r, &ctx, src_bits)?;
            let reg = ctx.reg(size, ctx.reg_num(&m));
            let rm = rm_operand(&ctx, &m, mem, src_bits);
            Ok(Decoded::new(name, vec![reg, rm]))
        }
        0xB8 => {
            if mp != Mandatory::F3 {
                return Err(());
            }
            let (reg, rm) = reg_rm(r, &ctx, size)?;
            Ok(Decoded::new("popcnt", vec![reg, rm]))
        }
        0xB9 => Err(()),
        0xBA => {
            let (m, mem) = modrm(r, &ctx, size)?;
            let name = match m.reg & 7 {
                4 => "bt",
                5 => "bts",
                6 => "btr",
                7 => "btc",
                _ => return Err(()),
            };
            let rm = rm_operand(&ctx, &m, mem, size);
            let imm = imm_operand(r, 8, 8, false)?;
            Ok(Decoded::new(name, vec![rm, imm]))
        }
        0xBC | 0xBD => {
            let name = match (op, mp) {
                (0xBC, _) if mp == Mandatory::F3 => "tzcnt",
                (0xBD, _) if mp == Mandatory::F3 => "lzcnt",
                (0xBC, _) => "bsf",
                _ => "bsr",
            };
            let (reg, rm) = reg_rm(r, &ctx, size)?;
            Ok(Decoded::new(name, vec![reg, rm]))
        }
        0xC0 | 0xC1 => {
            let size = if op == 0xC0 { 8 } else { size };
            let (rm, reg) = rm_reg(r, &ctx, size)?;
            Ok(Decoded::new("xadd", vec![rm, reg]))
        }
        0xC2 => {
            let name = match mp {
                Mandatory::None => "cmpps",
                Mandatory::P66 => "cmppd",
                Mandatory::F3 => "cmpss",
                Mandatory::F2 => "cmpsd",
            };
            let bits = match mp {
                Mandatory::F3 => 32,
                Mandatory::F2 => 64,
                _ => 128,
            };
            vec_ge_imm(r, &ctx, RegKind::Xmm, bits, name)
        }
        0xC3 => {
            let (rm, reg) = rm_reg(r, &ctx, size)?;
            Ok(Decoded::new("movnti", vec![rm, reg]))
        }
        0xC6 => {
            let name = if mp == Mandatory::P66 { "shufpd" } else { "shufps" };
            vec_ge_imm(r, &ctx, RegKind::Xmm, 128, name)
        }
        0xC7 => group9(r, &ctx),
        0xC8..=0xCF => {
            let num = (op & 7) + if ctx.p.b() { 8 } else { 0 };
            let size = if ctx.p.w() { 64 } else { 32 };
            Ok(Decoded::new("bswap", vec![ctx.reg(size, num)]))
        }
        0xD0..=0xF7 => packed_late(r, &ctx, op, mp),
        0xF8..=0xFE => packed_late(r, &ctx, op, mp),
        _ => Err(()),
    }
}

/// 返回条件码对应的 `cmovcc` 助记符。
fn cmov_name(condition: u8) -> &'static str {
    const CMOV: [&str; 16] = [
        "cmovo", "cmovno", "cmovb", "cmovae", "cmove", "cmovne", "cmovbe", "cmova", "cmovs", "cmovns", "cmovp", "cmovnp", "cmovl", "cmovge",
        "cmovle", "cmovg",
    ];
    CMOV[usize::from(condition & 15)]
}

/// 返回条件码对应的 `setcc` 助记符。
fn setcc_name(condition: u8) -> &'static str {
    const SETCC: [&str; 16] = [
        "seto", "setno", "setb", "setae", "sete", "setne", "setbe", "seta", "sets", "setns", "setp", "setnp", "setl", "setge", "setle", "setg",
    ];
    SETCC[usize::from(condition & 15)]
}

/// 返回强制前缀选定的向量寄存器族。
pub(super) fn packed_kind(mp: Mandatory) -> Result<RegKind, ()> {
    match mp {
        Mandatory::None => Ok(RegKind::Mm),
        Mandatory::P66 => Ok(RegKind::Xmm),
        _ => Err(()),
    }
}

/// 解码 `0F 10..0F 17` 的移动与解包指令。
fn mov_vector(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    let (name, bits, store) = match (op, mp) {
        (0x10, Mandatory::None) => ("movups", 128, false),
        (0x10, Mandatory::P66) => ("movupd", 128, false),
        (0x10, Mandatory::F3) => ("movss", 32, false),
        (0x10, Mandatory::F2) => ("movsd", 64, false),
        (0x11, Mandatory::None) => ("movups", 128, true),
        (0x11, Mandatory::P66) => ("movupd", 128, true),
        (0x11, Mandatory::F3) => ("movss", 32, true),
        (0x11, Mandatory::F2) => ("movsd", 64, true),
        (0x12, Mandatory::None) => ("movlps", 64, false),
        (0x12, Mandatory::P66) => ("movlpd", 64, false),
        (0x12, Mandatory::F3) => ("movsldup", 128, false),
        (0x12, Mandatory::F2) => ("movddup", 64, false),
        (0x13, Mandatory::None) => ("movlps", 64, true),
        (0x13, Mandatory::P66) => ("movlpd", 64, true),
        (0x14, Mandatory::None) => ("unpcklps", 128, false),
        (0x14, Mandatory::P66) => ("unpcklpd", 128, false),
        (0x15, Mandatory::None) => ("unpckhps", 128, false),
        (0x15, Mandatory::P66) => ("unpckhpd", 128, false),
        (0x16, Mandatory::None) => ("movhps", 64, false),
        (0x16, Mandatory::P66) => ("movhpd", 64, false),
        (0x16, Mandatory::F3) => ("movshdup", 128, false),
        (0x17, Mandatory::None) => ("movhps", 64, true),
        (0x17, Mandatory::P66) => ("movhpd", 64, true),
        _ => return Err(()),
    };
    if store { vec_eg(r, ctx, RegKind::Xmm, bits, name) } else { vec_ge(r, ctx, RegKind::Xmm, bits, name) }
}

/// 解码 `0F 18` 预取提示。
fn prefetch(r: &mut Reader<'_>, ctx: &Ctx) -> Result<Decoded, ()> {
    let (m, mem) = modrm(r, ctx, ctx.op_size)?;
    let Some(mem) = mem
    else {
        return Err(());
    };
    let name = match m.reg & 7 {
        0 => "nop",
        1 => "prefetchnta",
        2 => "prefetcht0",
        3 => "prefetcht1",
        4 => "prefetcht2",
        _ => return Err(()),
    };
    Ok(Decoded::new(name, vec![Operand::Mem(mem)]))
}

/// 解码 `0F 1E`，包含 `endbr32` 与 `endbr64`。
fn endbr(r: &mut Reader<'_>, ctx: &Ctx) -> Result<Decoded, ()> {
    if ctx.p.rep == Some(0xF3) {
        let byte = r.byte().ok_or(())?;
        return match byte {
            0xFA => Ok(Decoded::bare("endbr64")),
            0xFB => Ok(Decoded::bare("endbr32")),
            _ => Err(()),
        };
    }
    let (m, mem) = modrm(r, ctx, ctx.op_size)?;
    let rm = rm_operand(ctx, &m, mem, ctx.op_size);
    Ok(Decoded::new("nop", vec![rm]))
}

/// 解码 `0F 20..0F 23` 的通用寄存器与控制器/调试器寄存器移动。
fn mov_control(r: &mut Reader<'_>, ctx: &Ctx, op: u8) -> Result<Decoded, ()> {
    let (m, mem) = modrm(r, ctx, 0)?;
    if mem.is_some() {
        return Err(());
    }
    let gpr_bits = if ctx.bitness == 64 { 64 } else { 32 };
    let gpr = ctx.reg(gpr_bits, m.reg + if ctx.p.r() { 8 } else { 0 });
    let special_num = m.rm + if ctx.p.b() { 8 } else { 0 };
    let kind = if op == 0x20 || op == 0x22 { RegKind::Cr } else { RegKind::Dr };
    let special = Operand::Reg(Reg { kind, size: gpr_bits, num: special_num, rex: false });
    if op == 0x20 || op == 0x21 { Ok(Decoded::new("mov", vec![gpr, special])) } else { Ok(Decoded::new("mov", vec![special, gpr])) }
}

/// 解码 `0F 28` 与 `0F 29` 对齐 SSE 移动。
fn mov_aligned(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    let name = match mp {
        Mandatory::None => "movaps",
        Mandatory::P66 => "movapd",
        _ => return Err(()),
    };
    if op == 0x28 { vec_ge(r, ctx, RegKind::Xmm, 128, name) } else { vec_eg(r, ctx, RegKind::Xmm, 128, name) }
}
