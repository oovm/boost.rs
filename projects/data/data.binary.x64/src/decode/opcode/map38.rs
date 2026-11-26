//! 三字节 `0F 38` 与 `0F 3A` opcode map。

use crate::decode::{
    Decoded,
    operand::{Operand, RegKind},
    prefix::Reader,
};

use super::{
    Ctx, imm_operand,
    map0f::{Mandatory, mandatory, packed_kind, vec_ge, vec_ge_imm, vec_reg},
    modrm,
};

/// 解码三字节 opcode map（`0F 38` 或 `0F 3A`）。
pub(super) fn three_byte(r: &mut Reader<'_>, ctx: &Ctx, map: u8) -> Result<Decoded, ()> {
    let op = r.byte().ok_or(())?;
    let mp = mandatory(ctx);
    if map == 0x38 { map_38(r, ctx, op, mp) } else { map_3a(r, ctx, op, mp) }
}

/// 解码 `0F 38` map。
fn map_38(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    match op {
        0x00..=0x0B => {
            let kind = packed_kind(mp)?;
            let bits = if kind == RegKind::Mm { 64 } else { 128 };
            let name = match op {
                0x00 => "pshufb",
                0x01 => "phaddw",
                0x02 => "phaddd",
                0x03 => "phaddsw",
                0x04 => "pmaddubsw",
                0x05 => "phsubw",
                0x06 => "phsubd",
                0x07 => "phsubsw",
                0x08 => "psignb",
                0x09 => "psignw",
                0x0A => "psignd",
                _ => "pmulhrsw",
            };
            vec_ge(r, ctx, kind, bits, name)
        }
        0x10 => vec_ge_66(r, ctx, 128, "pblendvb"),
        0x14 => vec_ge_66(r, ctx, 128, "blendvps"),
        0x15 => vec_ge_66(r, ctx, 128, "blendvpd"),
        0x17 => vec_ge_66(r, ctx, 128, "ptest"),
        0x1C..=0x1E => {
            let kind = packed_kind(mp)?;
            let bits = if kind == RegKind::Mm { 64 } else { 128 };
            let name = match op {
                0x1C => "pabsb",
                0x1D => "pabsw",
                _ => "pabsd",
            };
            vec_ge(r, ctx, kind, bits, name)
        }
        0x20..=0x25 => pmovx(r, ctx, op, true),
        0x28 => vec_ge_66(r, ctx, 128, "pmuldq"),
        0x29 => vec_ge_66(r, ctx, 128, "pcmpeqq"),
        0x2A => vec_ge_66(r, ctx, 128, "movntdqa"),
        0x2B => vec_ge_66(r, ctx, 128, "packusdw"),
        0x30..=0x35 => pmovx(r, ctx, op, false),
        0x37 => vec_ge_66(r, ctx, 128, "pcmpgtq"),
        0x38..=0x3F => {
            let name = match op {
                0x38 => "pminsb",
                0x39 => "pminsd",
                0x3A => "pminuw",
                0x3B => "pminud",
                0x3C => "pmaxsb",
                0x3D => "pmaxsd",
                0x3E => "pmaxuw",
                _ => "pmaxud",
            };
            vec_ge_66(r, ctx, 128, name)
        }
        0x40 => vec_ge_66(r, ctx, 128, "pmulld"),
        0x41 => vec_ge_66(r, ctx, 128, "phminposuw"),
        0xF0 | 0xF1 => {
            let (m, mem) = modrm(r, ctx, ctx.op_size)?;
            let reg = ctx.reg(ctx.op_size, ctx.reg_num(&m));
            let Some(mem) = mem
            else {
                return Err(());
            };
            let rm = Operand::Mem(mem);
            if op == 0xF0 { Ok(Decoded::new("movbe", vec![reg, rm])) } else { Ok(Decoded::new("movbe", vec![rm, reg])) }
        }
        0xDB..=0xDF => {
            let name = match op {
                0xDB => "aesimc",
                0xDC => "aesenc",
                0xDD => "aesenclast",
                0xDE => "aesdec",
                _ => "aesdeclast",
            };
            vec_ge_66(r, ctx, 128, name)
        }
        _ => Err(()),
    }
}

/// 解码要求 `0x66` 前缀的 `reg, rm` 指令。
fn vec_ge_66(r: &mut Reader<'_>, ctx: &Ctx, bits: u32, name: &'static str) -> Result<Decoded, ()> {
    if mandatory(ctx) != Mandatory::P66 {
        return Err(());
    }
    vec_ge(r, ctx, RegKind::Xmm, bits, name)
}

/// 解码 `pmovsx*` 与 `pmovzx*` 加宽移动。
fn pmovx(r: &mut Reader<'_>, ctx: &Ctx, op: u8, signed: bool) -> Result<Decoded, ()> {
    let suffix = match op & 7 {
        0 => "bw",
        1 => "bd",
        2 => "bq",
        3 => "wd",
        4 => "wq",
        _ => "dq",
    };
    let name = if signed {
        match suffix {
            "bw" => "pmovsxbw",
            "bd" => "pmovsxbd",
            "bq" => "pmovsxbq",
            "wd" => "pmovsxwd",
            "wq" => "pmovsxwq",
            _ => "pmovsxdq",
        }
    }
    else {
        match suffix {
            "bw" => "pmovzxbw",
            "bd" => "pmovzxbd",
            "bq" => "pmovzxbq",
            "wd" => "pmovzxwd",
            "wq" => "pmovzxwq",
            _ => "pmovzxdq",
        }
    };
    let bits = match suffix {
        "bq" => 16,
        "bd" | "wq" => 32,
        _ => 64,
    };
    vec_ge_66(r, ctx, bits, name)
}

/// 解码 `0F 3A` map。
fn map_3a(r: &mut Reader<'_>, ctx: &Ctx, op: u8, mp: Mandatory) -> Result<Decoded, ()> {
    match op {
        0x08 => vec_ge_66(r, ctx, 128, "roundps"),
        0x09 => vec_ge_66(r, ctx, 128, "roundpd"),
        0x0A => vec_ge_66(r, ctx, 32, "roundss"),
        0x0B => vec_ge_66(r, ctx, 64, "roundsd"),
        0x0C => vec_ge_66(r, ctx, 128, "blendps"),
        0x0D => vec_ge_66(r, ctx, 128, "blendpd"),
        0x0E => vec_ge_66(r, ctx, 128, "pblendw"),
        0x0F => {
            let kind = packed_kind(mp)?;
            let bits = if kind == RegKind::Mm { 64 } else { 128 };
            vec_ge_imm(r, ctx, kind, bits, "palignr")
        }
        0x14 | 0x15 | 0x16 | 0x17 => extract(r, ctx, op),
        0x20 | 0x21 | 0x22 => insert(r, ctx, op),
        0x40 => vec_ge_66(r, ctx, 128, "dpps"),
        0x41 => vec_ge_66(r, ctx, 128, "dppd"),
        0x42 => vec_ge_66(r, ctx, 128, "mpsadbw"),
        0x44 => vec_ge_66(r, ctx, 128, "pclmulqdq"),
        0xDF => vec_ge_66(r, ctx, 128, "aeskeygenassist"),
        _ => Err(()),
    }
}

/// 解码 `pextrb`、`pextrw`、`pextrd`/`pextrq` 与 `extractps`。
fn extract(r: &mut Reader<'_>, ctx: &Ctx, op: u8) -> Result<Decoded, ()> {
    if mandatory(ctx) != Mandatory::P66 {
        return Err(());
    }
    let bits = match op {
        0x14 => 8,
        0x15 => 16,
        0x16 => {
            if ctx.p.w() {
                64
            }
            else {
                32
            }
        }
        _ => 32,
    };
    let name = match op {
        0x14 => "pextrb",
        0x15 => "pextrw",
        0x16 => {
            if bits == 64 {
                "pextrq"
            }
            else {
                "pextrd"
            }
        }
        _ => "extractps",
    };
    let (m, mem) = modrm(r, ctx, bits)?;
    let rm = match mem {
        Some(mem) => Operand::Mem(mem),
        None => ctx.reg(bits.max(32), ctx.rm_num(&m)),
    };
    let src = vec_reg(RegKind::Xmm, ctx.reg_num(&m));
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![rm, src, imm]))
}

/// 解码 `pinsrb`、`insertps` 与 `pinsrd`/`pinsrq`。
fn insert(r: &mut Reader<'_>, ctx: &Ctx, op: u8) -> Result<Decoded, ()> {
    if mandatory(ctx) != Mandatory::P66 {
        return Err(());
    }
    let bits = match op {
        0x20 => 8,
        0x21 => 32,
        _ => {
            if ctx.p.w() {
                64
            }
            else {
                32
            }
        }
    };
    let name = match op {
        0x20 => "pinsrb",
        0x21 => "insertps",
        _ => {
            if bits == 64 {
                "pinsrq"
            }
            else {
                "pinsrd"
            }
        }
    };
    let (m, mem) = modrm(r, ctx, bits)?;
    let dst = vec_reg(RegKind::Xmm, ctx.reg_num(&m));
    let src = match mem {
        Some(mem) => Operand::Mem(mem),
        None if op == 0x21 => vec_reg(RegKind::Xmm, ctx.rm_num(&m)),
        None => ctx.reg(bits.max(32), ctx.rm_num(&m)),
    };
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![dst, src, imm]))
}
