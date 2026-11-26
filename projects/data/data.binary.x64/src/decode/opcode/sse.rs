//! `0F` map 的 SSE 与打包整数辅助解码器。

use crate::decode::{
    Decoded,
    operand::{Operand, Reg, RegKind},
    prefix::Reader,
};

use super::{
    Ctx, imm_operand,
    map0f::{Mandatory, packed_kind, vec_eg, vec_ge, vec_ge_imm, vec_reg},
    modrm, rm_reg,
};

/// 解码 `0F 2A` 整数到标量浮点转换。
pub(super) fn cvt_int_to_float(r: &mut Reader<'_>, ctx: &Ctx, mp: Mandatory) -> Result<Decoded, ()> {
    let name = match mp {
        Mandatory::F3 => "cvtsi2ss",
        Mandatory::F2 => "cvtsi2sd",
        _ => return Err(()),
    };
    let src_bits = if ctx.p.w() { 64 } else { 32 };
    let (m, mem) = modrm(r, ctx, src_bits)?;
    let reg = vec_reg(RegKind::Xmm, ctx.reg_num(&m));
    let rm = match mem {
        Some(mem) => Operand::Mem(mem),
        None => ctx.reg(src_bits, ctx.rm_num(&m)),
    };
    Ok(Decoded::new(name, vec![reg, rm]))
}

/// 解码 `0F 2C` 与 `0F 2D` 标量浮点到整数转换。
pub(super) fn cvt_float_to_int(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    let name = match (op, mp) {
        (0x2C, Mandatory::F3) => "cvttss2si",
        (0x2C, Mandatory::F2) => "cvttsd2si",
        (_, Mandatory::F3) => "cvtss2si",
        (_, Mandatory::F2) => "cvtsd2si",
        _ => return Err(()),
    };
    let src_bits = if mp == Mandatory::F3 { 32 } else { 64 };
    let dst_bits = if ctx.p.w() { 64 } else { 32 };
    let (m, mem) = modrm(r, ctx, src_bits)?;
    let reg = ctx.reg(dst_bits, ctx.reg_num(&m));
    let rm = match mem {
        Some(mem) => Operand::Mem(mem),
        None => vec_reg(RegKind::Xmm, ctx.rm_num(&m)),
    };
    Ok(Decoded::new(name, vec![reg, rm]))
}

/// 解码 `0F 2E` 与 `0F 2F` 标量比较。
pub(super) fn compare_float(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    let name = match (op, mp) {
        (0x2E, Mandatory::None) => "ucomiss",
        (0x2E, Mandatory::P66) => "ucomisd",
        (0x2F, Mandatory::None) => "comiss",
        (0x2F, Mandatory::P66) => "comisd",
        _ => return Err(()),
    };
    let bits = if mp == Mandatory::None { 32 } else { 64 };
    vec_ge(r, ctx, RegKind::Xmm, bits, name)
}

/// 解码 `0F 50..0F 5F` 的 SSE 算术指令。
pub(super) fn sse_arith(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    match op {
        0x50 => {
            let name = if mp == Mandatory::P66 { "movmskpd" } else { "movmskps" };
            let (m, mem) = modrm(r, ctx, 128)?;
            if mem.is_some() {
                return Err(());
            }
            let dst_bits = if ctx.p.w() { 64 } else { 32 };
            let dst = ctx.reg(dst_bits, ctx.reg_num(&m));
            let src = vec_reg(RegKind::Xmm, ctx.rm_num(&m));
            return Ok(Decoded::new(name, vec![dst, src]));
        }
        _ => {}
    }
    let (name, bits) = match (op, mp) {
        (0x51, Mandatory::None) => ("sqrtps", 128),
        (0x51, Mandatory::P66) => ("sqrtpd", 128),
        (0x51, Mandatory::F3) => ("sqrtss", 32),
        (0x51, Mandatory::F2) => ("sqrtsd", 64),
        (0x52, Mandatory::None) => ("rsqrtps", 128),
        (0x52, Mandatory::F3) => ("rsqrtss", 32),
        (0x53, Mandatory::None) => ("rcpps", 128),
        (0x53, Mandatory::F3) => ("rcpss", 32),
        (0x54, Mandatory::None) => ("andps", 128),
        (0x54, Mandatory::P66) => ("andpd", 128),
        (0x55, Mandatory::None) => ("andnps", 128),
        (0x55, Mandatory::P66) => ("andnpd", 128),
        (0x56, Mandatory::None) => ("orps", 128),
        (0x56, Mandatory::P66) => ("orpd", 128),
        (0x57, Mandatory::None) => ("xorps", 128),
        (0x57, Mandatory::P66) => ("xorpd", 128),
        (0x58, Mandatory::None) => ("addps", 128),
        (0x58, Mandatory::P66) => ("addpd", 128),
        (0x58, Mandatory::F3) => ("addss", 32),
        (0x58, Mandatory::F2) => ("addsd", 64),
        (0x59, Mandatory::None) => ("mulps", 128),
        (0x59, Mandatory::P66) => ("mulpd", 128),
        (0x59, Mandatory::F3) => ("mulss", 32),
        (0x59, Mandatory::F2) => ("mulsd", 64),
        (0x5A, Mandatory::None) => ("cvtps2pd", 64),
        (0x5A, Mandatory::P66) => ("cvtpd2ps", 128),
        (0x5A, Mandatory::F3) => ("cvtss2sd", 32),
        (0x5A, Mandatory::F2) => ("cvtsd2ss", 64),
        (0x5B, Mandatory::None) => ("cvtdq2ps", 128),
        (0x5B, Mandatory::P66) => ("cvtps2dq", 128),
        (0x5B, Mandatory::F3) => ("cvttps2dq", 128),
        (0x5C, Mandatory::None) => ("subps", 128),
        (0x5C, Mandatory::P66) => ("subpd", 128),
        (0x5C, Mandatory::F3) => ("subss", 32),
        (0x5C, Mandatory::F2) => ("subsd", 64),
        (0x5D, Mandatory::None) => ("minps", 128),
        (0x5D, Mandatory::P66) => ("minpd", 128),
        (0x5D, Mandatory::F3) => ("minss", 32),
        (0x5D, Mandatory::F2) => ("minsd", 64),
        (0x5E, Mandatory::None) => ("divps", 128),
        (0x5E, Mandatory::P66) => ("divpd", 128),
        (0x5E, Mandatory::F3) => ("divss", 32),
        (0x5E, Mandatory::F2) => ("divsd", 64),
        (0x5F, Mandatory::None) => ("maxps", 128),
        (0x5F, Mandatory::P66) => ("maxpd", 128),
        (0x5F, Mandatory::F3) => ("maxss", 32),
        (0x5F, Mandatory::F2) => ("maxsd", 64),
        _ => return Err(()),
    };
    vec_ge(r, ctx, RegKind::Xmm, bits, name)
}

/// 解码 `0F 60..0F 6F` 的打包整数指令。
pub(super) fn packed(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    match op {
        0x60..=0x6B => {
            let kind = packed_kind(mp)?;
            let bits = if kind == RegKind::Mm { 64 } else { 128 };
            let name = match op {
                0x60 => "punpcklbw",
                0x61 => "punpcklwd",
                0x62 => "punpckldq",
                0x63 => "packsswb",
                0x64 => "pcmpgtb",
                0x65 => "pcmpgtw",
                0x66 => "pcmpgtd",
                0x67 => "packuswb",
                0x68 => "punpckhbw",
                0x69 => "punpckhwd",
                0x6A => "punpckhdq",
                _ => "packssdw",
            };
            vec_ge(r, ctx, kind, bits, name)
        }
        0x6C | 0x6D => {
            if mp != Mandatory::P66 {
                return Err(());
            }
            let name = if op == 0x6C { "punpcklqdq" } else { "punpckhqdq" };
            vec_ge(r, ctx, RegKind::Xmm, 128, name)
        }
        0x6E => {
            let bits = if ctx.p.w() { 64 } else { 32 };
            match mp {
                Mandatory::P66 => {
                    let (m, mem) = modrm(r, ctx, bits)?;
                    let reg = vec_reg(RegKind::Xmm, ctx.reg_num(&m));
                    let rm = match mem {
                        Some(mem) => Operand::Mem(mem),
                        None => ctx.reg(bits, ctx.rm_num(&m)),
                    };
                    Ok(Decoded::new(if bits == 64 { "movq" } else { "movd" }, vec![reg, rm]))
                }
                Mandatory::None => {
                    let (m, mem) = modrm(r, ctx, 32)?;
                    let reg = vec_reg(RegKind::Mm, m.reg & 7);
                    let rm = match mem {
                        Some(mem) => Operand::Mem(mem),
                        None => ctx.reg(32, m.rm & 7),
                    };
                    Ok(Decoded::new("movd", vec![reg, rm]))
                }
                _ => Err(()),
            }
        }
        0x6F => match mp {
            Mandatory::None => vec_ge(r, ctx, RegKind::Mm, 64, "movq"),
            Mandatory::P66 => vec_ge(r, ctx, RegKind::Xmm, 128, "movdqa"),
            Mandatory::F3 => vec_ge(r, ctx, RegKind::Xmm, 128, "movdqu"),
            Mandatory::F2 => Err(()),
        },
        _ => Err(()),
    }
}

/// 解码 `0F 70` shuffle 指令。
pub(super) fn shuffle(r: &mut Reader<'_>, ctx: &Ctx, mp: Mandatory) -> Result<Decoded, ()> {
    let (name, kind, bits) = match mp {
        Mandatory::None => ("pshufw", RegKind::Mm, 64),
        Mandatory::P66 => ("pshufd", RegKind::Xmm, 128),
        Mandatory::F3 => ("pshufhw", RegKind::Xmm, 128),
        Mandatory::F2 => ("pshuflw", RegKind::Xmm, 128),
    };
    vec_ge_imm(r, ctx, kind, bits, name)
}

/// 解码 `0F 71..0F 73` 移位组。
pub(super) fn shift_group(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    let kind = packed_kind(mp)?;
    let bits = if kind == RegKind::Mm { 64 } else { 128 };
    let (m, mem) = modrm(r, ctx, bits)?;
    let name = match (op, m.reg & 7) {
        (0x71, 2) => "psrlw",
        (0x71, 4) => "psraw",
        (0x71, 6) => "psllw",
        (0x72, 2) => "psrld",
        (0x72, 4) => "psrad",
        (0x72, 6) => "pslld",
        (0x73, 2) => "psrlq",
        (0x73, 3) => "psrldq",
        (0x73, 6) => "psllq",
        (0x73, 7) => "pslldq",
        _ => return Err(()),
    };
    let rm = match mem {
        Some(mem) => Operand::Mem(mem),
        None => vec_reg(kind, ctx.rm_num(&m)),
    };
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![rm, imm]))
}

/// 解码 `0F 7E` 与 `0F 7F` 向量移动。
pub(super) fn move_special(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    if op == 0x7F {
        return match mp {
            Mandatory::None => vec_eg(r, ctx, RegKind::Mm, 64, "movq"),
            Mandatory::P66 => vec_eg(r, ctx, RegKind::Xmm, 128, "movdqa"),
            Mandatory::F3 => vec_eg(r, ctx, RegKind::Xmm, 128, "movdqu"),
            Mandatory::F2 => Err(()),
        };
    }
    match mp {
        Mandatory::P66 => {
            let bits = if ctx.p.w() { 64 } else { 32 };
            let (m, mem) = modrm(r, ctx, bits)?;
            let rm = match mem {
                Some(mem) => Operand::Mem(mem),
                None => ctx.reg(bits, ctx.rm_num(&m)),
            };
            let reg = vec_reg(RegKind::Xmm, ctx.reg_num(&m));
            Ok(Decoded::new(if bits == 64 { "movq" } else { "movd" }, vec![rm, reg]))
        }
        Mandatory::F3 => vec_ge(r, ctx, RegKind::Xmm, 64, "movq"),
        Mandatory::None => {
            let (m, mem) = modrm(r, ctx, 32)?;
            let rm = match mem {
                Some(mem) => Operand::Mem(mem),
                None => ctx.reg(32, m.rm & 7),
            };
            let reg = vec_reg(RegKind::Mm, m.reg & 7);
            Ok(Decoded::new("movd", vec![rm, reg]))
        }
        Mandatory::F2 => Err(()),
    }
}

/// 解码 `shld` 与 `shrd`。
pub(super) fn shift_double(r: &mut Reader<'_>, ctx: &Ctx, op: u8) -> Result<Decoded, ()> {
    let name = if op < 0xA8 { "shld" } else { "shrd" };
    let (rm, reg) = rm_reg(r, ctx, ctx.op_size)?;
    if op & 1 == 1 {
        Ok(Decoded::new(name, vec![rm, reg, Operand::Reg(Reg::gpr(8, 1, ctx.p.has_rex()))]))
    }
    else {
        let imm = imm_operand(r, 8, 8, false)?;
        Ok(Decoded::new(name, vec![rm, reg, imm]))
    }
}

/// 解码 `lss`、`lfs` 与 `lgs`。
pub(super) fn system_ptr(r: &mut Reader<'_>, ctx: &Ctx, name: &'static str) -> Result<Decoded, ()> {
    let (m, mem) = modrm(r, ctx, 0)?;
    let Some(mem) = mem
    else {
        return Err(());
    };
    Ok(Decoded::new(name, vec![ctx.reg(ctx.op_size, ctx.reg_num(&m)), Operand::Mem(mem)]))
}

/// 解码 `0F AE` 组。
pub(super) fn group15(r: &mut Reader<'_>, ctx: &Ctx) -> Result<Decoded, ()> {
    let (m, mem) = modrm(r, ctx, 0)?;
    match mem {
        Some(mut mem) => {
            let (name, bits) = match m.reg & 7 {
                0 => ("fxsave", 512),
                1 => ("fxrstor", 512),
                2 => ("ldmxcsr", 32),
                3 => ("stmxcsr", 32),
                4 => ("xsave", 512),
                5 => ("xrstor", 512),
                6 => ("xsaveopt", 512),
                _ => ("clflush", 8),
            };
            mem.size = bits;
            Ok(Decoded::new(name, vec![Operand::Mem(mem)]))
        }
        None => {
            let name = match m.reg & 7 {
                5 => "lfence",
                6 => "mfence",
                7 => "sfence",
                _ => return Err(()),
            };
            Ok(Decoded::bare(name))
        }
    }
}

/// 解码 `0F C7` 组。
pub(super) fn group9(r: &mut Reader<'_>, ctx: &Ctx) -> Result<Decoded, ()> {
    let (m, mem) = modrm(r, ctx, 0)?;
    match m.reg & 7 {
        1 => {
            let Some(mut mem) = mem
            else {
                return Err(());
            };
            let (name, bits) = if ctx.p.w() { ("cmpxchg16b", 128) } else { ("cmpxchg8b", 64) };
            mem.size = bits;
            Ok(Decoded::new(name, vec![Operand::Mem(mem)]))
        }
        6 | 7 => {
            if mem.is_some() {
                return Err(());
            }
            let bits = if ctx.p.w() { 64 } else { 32 };
            let name = if m.reg & 7 == 6 { "rdrand" } else { "rdseed" };
            Ok(Decoded::new(name, vec![ctx.reg(bits, ctx.rm_num(&m))]))
        }
        _ => Err(()),
    }
}

/// 解码 `0F D0..0F FE` 中的打包整数指令。
pub(super) fn packed_late(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    match op {
        0xD0 => {
            let name = match mp {
                Mandatory::F2 => "addsubps",
                Mandatory::P66 => "addsubpd",
                _ => return Err(()),
            };
            return vec_ge(r, ctx, RegKind::Xmm, 128, name);
        }
        0xD6 => {
            return match mp {
                Mandatory::P66 => vec_eg(r, ctx, RegKind::Xmm, 64, "movq"),
                Mandatory::None => vec_eg(r, ctx, RegKind::Mm, 64, "movq"),
                _ => Err(()),
            };
        }
        0xD7 => {
            let (m, mem) = modrm(r, ctx, if mp == Mandatory::P66 { 128 } else { 64 })?;
            if mem.is_some() {
                return Err(());
            }
            let dst_bits = if ctx.p.w() { 64 } else { 32 };
            let dst = ctx.reg(dst_bits, ctx.reg_num(&m));
            let src = match mp {
                Mandatory::P66 => vec_reg(RegKind::Xmm, ctx.rm_num(&m)),
                Mandatory::None => vec_reg(RegKind::Mm, m.rm & 7),
                _ => return Err(()),
            };
            return Ok(Decoded::new("pmovmskb", vec![dst, src]));
        }
        0xE6 => {
            let name = match mp {
                Mandatory::F2 => "cvtpd2dq",
                Mandatory::P66 => "cvttpd2dq",
                Mandatory::F3 => "cvtdq2pd",
                _ => return Err(()),
            };
            return vec_ge(r, ctx, RegKind::Xmm, 128, name);
        }
        0xE7 => {
            if mp != Mandatory::P66 {
                return Err(());
            }
            return vec_eg(r, ctx, RegKind::Xmm, 128, "movntdq");
        }
        0xF0 => {
            if mp != Mandatory::F2 {
                return Err(());
            }
            return vec_ge(r, ctx, RegKind::Xmm, 128, "lddqu");
        }
        0xF7 => {
            return match mp {
                Mandatory::P66 => vec_ge(r, ctx, RegKind::Xmm, 128, "maskmovdqu"),
                Mandatory::None => vec_ge(r, ctx, RegKind::Mm, 64, "maskmovq"),
                _ => Err(()),
            };
        }
        _ => {}
    }
    let kind = packed_kind(mp)?;
    let bits = if kind == RegKind::Mm { 64 } else { 128 };
    let name = match op {
        0xD1 => "psrlw",
        0xD2 => "psrld",
        0xD3 => "psrlq",
        0xD4 => "paddq",
        0xD5 => "pmullw",
        0xD8 => "psubusb",
        0xD9 => "psubusw",
        0xDA => "pminub",
        0xDB => "pand",
        0xDC => "paddusb",
        0xDD => "paddusw",
        0xDE => "pmaxub",
        0xDF => "pandn",
        0xE0 => "pavgb",
        0xE1 => "psraw",
        0xE2 => "psrad",
        0xE3 => "pavgw",
        0xE4 => "pmulhuw",
        0xE5 => "pmulhw",
        0xE8 => "psubsb",
        0xE9 => "psubsw",
        0xEA => "pminsw",
        0xEB => "por",
        0xEC => "paddsb",
        0xED => "paddsw",
        0xEE => "pmaxsw",
        0xEF => "pxor",
        0xF1 => "psllw",
        0xF2 => "pslld",
        0xF3 => "psllq",
        0xF4 => "pmuludq",
        0xF5 => "pmaddwd",
        0xF6 => "psadbw",
        0xF8 => "psubb",
        0xF9 => "psubw",
        0xFA => "psubd",
        0xFB => "psubq",
        0xFC => "paddb",
        0xFD => "paddw",
        _ => "paddd",
    };
    vec_ge(r, ctx, kind, bits, name)
}
