//! VEX 与 EVEX 编码的向量指令。

mod maps;

use self::maps::{evex_map1, map2, map3};
use crate::decode::{
    Decoded,
    modrm::{self, ModRm},
    opcode::{Ctx, imm_operand},
    operand::{Mem, Operand, Reg, RegKind},
    prefix::Reader,
};

/// 已解码的 VEX 或 EVEX 前缀字段。
#[derive(Clone, Copy)]
struct Vex {
    /// opcode map：`1` 为 `0F`，`2` 为 `0F 38`，`3` 为 `0F 3A`。
    map: u8,
    /// 强制前缀：`0` 无，`1` `0x66`，`2` `0xF3`，`3` `0xF2`。
    pp: u8,
    /// 向量长度：`0` 128 位，`1` 256 位，`2` 512 位。
    l: u8,
    /// W 位。
    w: bool,
    /// 非破坏性源寄存器。
    vvvv: u8,
    /// ModRM reg 字段的扩展位（8）。
    r: bool,
    /// SIB index 字段的扩展位。
    x: bool,
    /// ModRM rm 字段的扩展位。
    b: bool,
    /// ModRM reg 字段的 EVEX 扩展位（16）。
    r2: bool,
    /// 是否使用 EVEX 前缀。
    evex: bool,
}

impl Vex {
    /// 长度字段选定的向量寄存器族。
    fn kind(&self) -> RegKind {
        match self.l {
            0 => RegKind::Xmm,
            1 => RegKind::Ymm,
            _ => RegKind::Zmm,
        }
    }

    /// 向量宽度（位）。
    fn bits(&self) -> u32 {
        128 << self.l
    }
}

/// 构造向量寄存器操作数。
fn vreg(kind: RegKind, num: u8) -> Operand {
    Operand::Reg(Reg::vec(kind, num))
}

/// 构造 ModRM reg 操作数。
fn reg_operand(v: &Vex, m: &ModRm) -> Operand {
    let num = m.reg + if v.r { 8 } else { 0 } + if v.evex && v.r2 { 16 } else { 0 };
    vreg(v.kind(), num)
}

/// 构造 ModRM rm 在选中寄存器时的操作数。
fn rm_operand(v: &Vex, m: &ModRm) -> Operand {
    let num = m.rm + if v.b { 8 } else { 0 } + if v.evex && v.x { 16 } else { 0 };
    vreg(v.kind(), num)
}

/// 构造非破坏性源操作数。
fn vvvv_operand(v: &Vex) -> Operand {
    vreg(v.kind(), v.vvvv)
}

/// 用 VEX 位重写前缀上下文，让共享的 ModRM 辅助函数看到它们。
fn vex_ctx(ctx: &Ctx, v: &Vex) -> Ctx {
    let mut p = ctx.p;
    p.rex = (if v.w { 8 } else { 0 }) | (if v.r { 4 } else { 0 }) | (if v.x { 2 } else { 0 }) | (if v.b { 1 } else { 0 });
    p.opsize = v.pp == 1;
    p.rep = match v.pp {
        2 => Some(0xF3),
        3 => Some(0xF2),
        _ => None,
    };
    Ctx::new(ctx.bitness, p, ctx.start, ctx.ip)
}

/// 读取向量指令的 ModRM 字节。
fn read_rm(r: &mut Reader<'_>, ctx: &Ctx, bits: u32) -> Result<(ModRm, Option<Mem>), ()> {
    modrm::read_modrm(r, bits, ctx.bitness, &ctx.p)
}

/// 解码双操作数 `reg, rm` 向量指令。
fn v2(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, name: &'static str, bits: u32) -> Result<Decoded, ()> {
    let (m, mem) = read_rm(r, ctx, bits)?;
    let dst = reg_operand(v, &m);
    let src = match mem {
        Some(mem) => Operand::Mem(mem),
        None => rm_operand(v, &m),
    };
    Ok(Decoded::new(name, vec![dst, src]))
}

/// 解码双操作数 `rm, reg` 向量指令。
fn v2_store(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, name: &'static str, bits: u32) -> Result<Decoded, ()> {
    let (m, mem) = read_rm(r, ctx, bits)?;
    let src = reg_operand(v, &m);
    let dst = match mem {
        Some(mem) => Operand::Mem(mem),
        None => rm_operand(v, &m),
    };
    Ok(Decoded::new(name, vec![dst, src]))
}

/// 解码三操作数 `reg, vvvv, rm` 向量指令。
fn v3(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, name: &'static str, bits: u32) -> Result<Decoded, ()> {
    let (m, mem) = read_rm(r, ctx, bits)?;
    let dst = reg_operand(v, &m);
    let src2 = vvvv_operand(v);
    let src1 = match mem {
        Some(mem) => Operand::Mem(mem),
        None => rm_operand(v, &m),
    };
    Ok(Decoded::new(name, vec![dst, src2, src1]))
}

/// 解码 `reg, rm, imm8` 向量指令。
fn v2_imm(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, name: &'static str, bits: u32) -> Result<Decoded, ()> {
    let (m, mem) = read_rm(r, ctx, bits)?;
    let dst = reg_operand(v, &m);
    let src = match mem {
        Some(mem) => Operand::Mem(mem),
        None => rm_operand(v, &m),
    };
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![dst, src, imm]))
}

/// 解码 `reg, vvvv, rm, imm8` 向量指令。
fn v3_imm(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, name: &'static str, bits: u32) -> Result<Decoded, ()> {
    let (m, mem) = read_rm(r, ctx, bits)?;
    let dst = reg_operand(v, &m);
    let src2 = vvvv_operand(v);
    let src1 = match mem {
        Some(mem) => Operand::Mem(mem),
        None => rm_operand(v, &m),
    };
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![dst, src2, src1, imm]))
}

/// 解码两字节或三字节 VEX 前缀及其指令。
pub(crate) fn decode_vex(r: &mut Reader<'_>, ctx: Ctx, two_byte: bool) -> Result<Decoded, ()> {
    let mut v = Vex { map: 1, pp: 0, l: 0, w: false, vvvv: 0, r: false, x: false, b: false, r2: false, evex: false };
    if two_byte {
        let byte = r.byte().ok_or(())?;
        v.r = byte & 0x80 == 0;
        v.vvvv = (!(byte >> 3)) & 0xF;
        v.l = (byte >> 2) & 1;
        v.pp = byte & 3;
    }
    else {
        let b2 = r.byte().ok_or(())?;
        let b3 = r.byte().ok_or(())?;
        v.r = b2 & 0x80 == 0;
        v.x = b2 & 0x40 == 0;
        v.b = b2 & 0x20 == 0;
        v.map = b2 & 0x1F;
        v.w = b3 & 0x80 != 0;
        v.vvvv = (!(b3 >> 3)) & 0xF;
        v.l = (b3 >> 2) & 1;
        v.pp = b3 & 3;
        if v.map < 1 || v.map > 3 {
            return Err(());
        }
    }
    let inner = vex_ctx(&ctx, &v);
    let op = r.byte().ok_or(())?;
    match v.map {
        1 => map1(r, &inner, &v, op),
        2 => map2(r, &inner, &v, op),
        _ => map3(r, &inner, &v, op),
    }
}

/// 解码 EVEX 前缀及其指令。
pub(crate) fn decode_evex(r: &mut Reader<'_>, ctx: Ctx) -> Result<Decoded, ()> {
    let p0 = r.byte().ok_or(())?;
    let p1 = r.byte().ok_or(())?;
    let p2 = r.byte().ok_or(())?;
    if p0 & 0x0C != 0 {
        return Err(());
    }
    let map = p0 & 3;
    if map == 0 {
        return Err(());
    }
    if p1 & 0x04 == 0 {
        return Err(());
    }
    let ll = (p2 >> 5) & 3;
    if ll == 3 {
        return Err(());
    }
    let v = Vex {
        map,
        pp: p1 & 3,
        l: ll,
        w: p1 & 0x80 != 0,
        vvvv: ((!(p1 >> 3)) & 0xF) + if p2 & 0x08 == 0 { 16 } else { 0 },
        r: p0 & 0x10 == 0,
        x: p0 & 0x40 == 0,
        b: p0 & 0x20 == 0,
        r2: p0 & 0x80 == 0,
        evex: true,
    };
    let inner = vex_ctx(&ctx, &v);
    let op = r.byte().ok_or(())?;
    if map != 1 {
        return Err(());
    }
    evex_map1(r, &inner, &v, op)
}

/// 返回强制前缀对应的标量名，或 `None`。
fn scalar_name(op: u8, pp: u8) -> Option<(&'static str, u32)> {
    let table: &[(&'static str, &'static str, &'static str, &'static str)] = match op {
        0x58 => &[("vaddps", "vaddpd", "vaddss", "vaddsd")],
        0x59 => &[("vmulps", "vmulpd", "vmulss", "vmulsd")],
        0x5C => &[("vsubps", "vsubpd", "vsubss", "vsubsd")],
        0x5D => &[("vminps", "vminpd", "vminss", "vminsd")],
        0x5E => &[("vdivps", "vdivpd", "vdivss", "vdivsd")],
        0x5F => &[("vmaxps", "vmaxpd", "vmaxss", "vmaxsd")],
        _ => return None,
    };
    let entry = table[0];
    let (name, bits) = match pp {
        0 => (entry.0, 128),
        1 => (entry.1, 128),
        2 => (entry.2, 32),
        _ => (entry.3, 64),
    };
    Some((name, bits))
}

/// 解码 VEX `0F` map。
fn map1(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let bits = v.bits();
    match op {
        0x10 | 0x11 => vex_move(r, ctx, v, op),
        0x28 | 0x29 => {
            let name = match v.pp {
                0 => "vmovaps",
                1 => "vmovapd",
                _ => return Err(()),
            };
            if op == 0x28 { v2(r, ctx, v, name, bits) } else { v2_store(r, ctx, v, name, bits) }
        }
        0x2A => vex_cvtsi2f(r, ctx, v),
        0x2B => {
            let name = match v.pp {
                0 => "vmovntps",
                1 => "vmovntpd",
                _ => return Err(()),
            };
            v2_store(r, ctx, v, name, bits)
        }
        0x2C | 0x2D => vex_cvtf2si(r, ctx, v, op),
        0x2E | 0x2F => {
            let name = match (op, v.pp) {
                (0x2E, 0) => "vucomiss",
                (0x2E, 1) => "vucomisd",
                (0x2F, 0) => "vcomiss",
                (0x2F, 1) => "vcomisd",
                _ => return Err(()),
            };
            let bits = if v.pp == 0 { 32 } else { 64 };
            v2(r, ctx, v, name, bits)
        }
        0x50 => {
            let name = if v.pp == 1 { "vmovmskpd" } else { "vmovmskps" };
            let (m, mem) = read_rm(r, ctx, bits)?;
            if mem.is_some() {
                return Err(());
            }
            let dst_bits = if v.w { 64 } else { 32 };
            let dst = Operand::Reg(Reg::gpr(dst_bits, m.reg + if v.r { 8 } else { 0 }, false));
            Ok(Decoded::new(name, vec![dst, rm_operand(v, &m)]))
        }
        0x51 => vex_sqrt(r, ctx, v, op),
        0x52 | 0x53 => vex_rsqrt(r, ctx, v, op),
        0x54..=0x5F => vex_arith(r, ctx, v, op),
        0x60..=0x6F => vex_packed(r, ctx, v, op),
        0x70 => {
            let name = match v.pp {
                1 => "vpshufd",
                2 => "vpshufhw",
                3 => "vpshuflw",
                _ => return Err(()),
            };
            v2_imm(r, ctx, v, name, bits)
        }
        0x71..=0x73 => vex_shift_group(r, ctx, v, op),
        0x74..=0x76 => {
            let name = match op {
                0x74 => "vpcmpeqb",
                0x75 => "vpcmpeqw",
                _ => "vpcmpeqd",
            };
            v3(r, ctx, v, name, bits)
        }
        0x77 => match (v.pp, v.l, v.vvvv) {
            (0, 0, 0) => Ok(Decoded::bare("vzeroupper")),
            (0, 1, 0) => Ok(Decoded::bare("vzeroall")),
            _ => Err(()),
        },
        0x7E | 0x7F => vex_move_special(r, ctx, v, op),
        0xC2 => {
            let name = match v.pp {
                0 => "vcmpps",
                1 => "vcmppd",
                2 => "vcmpss",
                _ => "vcmpsd",
            };
            let bits = match v.pp {
                2 => 32,
                3 => 64,
                _ => bits,
            };
            v3_imm(r, ctx, v, name, bits)
        }
        0xC6 => {
            let name = if v.pp == 1 { "vshufpd" } else { "vshufps" };
            v3_imm(r, ctx, v, name, bits)
        }
        0xD4 => v3(r, ctx, v, "vpaddq", bits),
        0xD7 => {
            let (m, mem) = read_rm(r, ctx, bits)?;
            if mem.is_some() {
                return Err(());
            }
            let dst_bits = if v.w { 64 } else { 32 };
            let dst = Operand::Reg(Reg::gpr(dst_bits, m.reg + if v.r { 8 } else { 0 }, false));
            Ok(Decoded::new("vpmovmskb", vec![dst, rm_operand(v, &m)]))
        }
        0xDB => v3(r, ctx, v, "vpand", bits),
        0xDF => v3(r, ctx, v, "vpandn", bits),
        0xEB => v3(r, ctx, v, "vpor", bits),
        0xEF => v3(r, ctx, v, "vpxor", bits),
        0xE7 => v2_store(r, ctx, v, "vmovntdq", bits),
        0xF8 => v3(r, ctx, v, "vpsubb", bits),
        0xF9 => v3(r, ctx, v, "vpsubw", bits),
        0xFA => v3(r, ctx, v, "vpsubd", bits),
        0xFB => v3(r, ctx, v, "vpsubq", bits),
        0xFC => v3(r, ctx, v, "vpaddb", bits),
        0xFD => v3(r, ctx, v, "vpaddw", bits),
        0xFE => v3(r, ctx, v, "vpaddd", bits),
        _ => Err(()),
    }
}

/// 解码 VEX `vmov*` 家族。
fn vex_move(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let (name, bits, scalar) = match (op, v.pp) {
        (0x10, 0) => ("vmovups", v.bits(), false),
        (0x10, 1) => ("vmovupd", v.bits(), false),
        (0x10, 2) => ("vmovss", 32, true),
        (0x10, 3) => ("vmovsd", 64, true),
        (0x11, 0) => ("vmovups", v.bits(), false),
        (0x11, 1) => ("vmovupd", v.bits(), false),
        (0x11, 2) => ("vmovss", 32, true),
        (0x11, 3) => ("vmovsd", 64, true),
        _ => return Err(()),
    };
    if op == 0x11 {
        return v2_store(r, ctx, v, name, bits);
    }
    if scalar {
        let (m, mem) = read_rm(r, ctx, bits)?;
        let dst = vreg(RegKind::Xmm, m.reg + if v.r { 8 } else { 0 });
        let src2 = vreg(RegKind::Xmm, v.vvvv);
        let src1 = match mem {
            Some(mem) => Operand::Mem(mem),
            None => vreg(RegKind::Xmm, m.rm + if v.b { 8 } else { 0 }),
        };
        return Ok(Decoded::new(name, vec![dst, src2, src1]));
    }
    v2(r, ctx, v, name, bits)
}

/// 解码 VEX 整数到标量浮点转换。
fn vex_cvtsi2f(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex) -> Result<Decoded, ()> {
    let name = match v.pp {
        2 => "vcvtsi2ss",
        3 => "vcvtsi2sd",
        _ => return Err(()),
    };
    let src_bits = if v.w { 64 } else { 32 };
    let (m, mem) = read_rm(r, ctx, src_bits)?;
    let dst = vreg(RegKind::Xmm, m.reg + if v.r { 8 } else { 0 });
    let src2 = vreg(RegKind::Xmm, v.vvvv);
    let src1 = match mem {
        Some(mem) => Operand::Mem(mem),
        None => Operand::Reg(Reg::gpr(src_bits, m.rm + if v.b { 8 } else { 0 }, false)),
    };
    Ok(Decoded::new(name, vec![dst, src2, src1]))
}

/// 解码 VEX 标量浮点到整数转换。
fn vex_cvtf2si(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let (name, src_bits) = match (op, v.pp) {
        (0x2C, 2) => ("vcvttss2si", 32),
        (0x2C, 3) => ("vcvttsd2si", 64),
        (0x2D, 2) => ("vcvtss2si", 32),
        (0x2D, 3) => ("vcvtsd2si", 64),
        _ => return Err(()),
    };
    let dst_bits = if v.w { 64 } else { 32 };
    let (m, mem) = read_rm(r, ctx, src_bits)?;
    let dst = Operand::Reg(Reg::gpr(dst_bits, m.reg + if v.r { 8 } else { 0 }, false));
    let src = match mem {
        Some(mem) => Operand::Mem(mem),
        None => vreg(RegKind::Xmm, m.rm + if v.b { 8 } else { 0 }),
    };
    Ok(Decoded::new(name, vec![dst, src]))
}

/// 解码 VEX 平方根指令。
fn vex_sqrt(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let _ = op;
    match v.pp {
        0 => v2(r, ctx, v, "vsqrtps", v.bits()),
        1 => v2(r, ctx, v, "vsqrtpd", v.bits()),
        2 => {
            let (m, mem) = read_rm(r, ctx, 32)?;
            let dst = vreg(RegKind::Xmm, m.reg + if v.r { 8 } else { 0 });
            let src2 = vreg(RegKind::Xmm, v.vvvv);
            let src1 = match mem {
                Some(mem) => Operand::Mem(mem),
                None => vreg(RegKind::Xmm, m.rm + if v.b { 8 } else { 0 }),
            };
            Ok(Decoded::new("vsqrtss", vec![dst, src2, src1]))
        }
        _ => {
            let (m, mem) = read_rm(r, ctx, 64)?;
            let dst = vreg(RegKind::Xmm, m.reg + if v.r { 8 } else { 0 });
            let src2 = vreg(RegKind::Xmm, v.vvvv);
            let src1 = match mem {
                Some(mem) => Operand::Mem(mem),
                None => vreg(RegKind::Xmm, m.rm + if v.b { 8 } else { 0 }),
            };
            Ok(Decoded::new("vsqrtsd", vec![dst, src2, src1]))
        }
    }
}

/// 解码 VEX 倒数指令。
fn vex_rsqrt(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let packed = if op == 0x52 { "vrsqrtps" } else { "vrcpps" };
    let scalar = if op == 0x52 { "vrsqrtss" } else { "vrcpss" };
    match v.pp {
        0 => v2(r, ctx, v, packed, v.bits()),
        2 => {
            let (m, mem) = read_rm(r, ctx, 32)?;
            let dst = vreg(RegKind::Xmm, m.reg + if v.r { 8 } else { 0 });
            let src2 = vreg(RegKind::Xmm, v.vvvv);
            let src1 = match mem {
                Some(mem) => Operand::Mem(mem),
                None => vreg(RegKind::Xmm, m.rm + if v.b { 8 } else { 0 }),
            };
            Ok(Decoded::new(scalar, vec![dst, src2, src1]))
        }
        _ => Err(()),
    }
}

/// 解码 VEX 打包与标量算术。
fn vex_arith(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    if let Some((name, bits)) = scalar_name(op, v.pp) {
        return v3(r, ctx, v, name, bits);
    }
    let bits = v.bits();
    let (name, scalar_bits) = match (op, v.pp) {
        (0x54, 0) => ("vandps", 0),
        (0x54, 1) => ("vandpd", 0),
        (0x55, 0) => ("vandnps", 0),
        (0x55, 1) => ("vandnpd", 0),
        (0x56, 0) => ("vorps", 0),
        (0x56, 1) => ("vorpd", 0),
        (0x57, 0) => ("vxorps", 0),
        (0x57, 1) => ("vxorpd", 0),
        (0x5A, 0) => ("vcvtps2pd", 0),
        (0x5A, 1) => ("vcvtpd2ps", 0),
        (0x5A, 2) => ("vcvtss2sd", 32),
        (0x5A, 3) => ("vcvtsd2ss", 64),
        (0x5B, 0) => ("vcvtdq2ps", 0),
        (0x5B, 1) => ("vcvtps2dq", 0),
        (0x5B, 2) => ("vcvttps2dq", 0),
        _ => return Err(()),
    };
    if scalar_bits == 0 { v3(r, ctx, v, name, bits) } else { v3(r, ctx, v, name, scalar_bits) }
}

/// 解码 VEX 打包整数指令。
fn vex_packed(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let bits = v.bits();
    match op {
        0x60..=0x6D => {
            if v.pp != 1 {
                return Err(());
            }
            let name = match op {
                0x60 => "vpunpcklbw",
                0x61 => "vpunpcklwd",
                0x62 => "vpunpckldq",
                0x63 => "vpacksswb",
                0x64 => "vpcmpgtb",
                0x65 => "vpcmpgtw",
                0x66 => "vpcmpgtd",
                0x67 => "vpackuswb",
                0x68 => "vpunpckhbw",
                0x69 => "vpunpckhwd",
                0x6A => "vpunpckhdq",
                0x6B => "vpackssdw",
                0x6C => "vpunpcklqdq",
                _ => "vpunpckhqdq",
            };
            v3(r, ctx, v, name, bits)
        }
        0x6E => {
            if v.pp != 1 {
                return Err(());
            }
            let src_bits = if v.w { 64 } else { 32 };
            let (m, mem) = read_rm(r, ctx, src_bits)?;
            let dst = vreg(RegKind::Xmm, m.reg + if v.r { 8 } else { 0 });
            let src = match mem {
                Some(mem) => Operand::Mem(mem),
                None => Operand::Reg(Reg::gpr(src_bits, m.rm + if v.b { 8 } else { 0 }, false)),
            };
            Ok(Decoded::new(if src_bits == 64 { "vmovq" } else { "vmovd" }, vec![dst, src]))
        }
        0x6F => match v.pp {
            1 => v2(r, ctx, v, "vmovdqa", bits),
            2 => v2(r, ctx, v, "vmovdqu", bits),
            0 => v2(r, ctx, v, "vmovq", 64),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

/// 解码 VEX 打包移位组。
fn vex_shift_group(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    if v.pp != 1 {
        return Err(());
    }
    let bits = v.bits();
    let (m, mem) = read_rm(r, ctx, bits)?;
    let name = match (op, m.reg & 7) {
        (0x71, 2) => "vpsrlw",
        (0x71, 4) => "vpsraw",
        (0x71, 6) => "vpsllw",
        (0x72, 2) => "vpsrld",
        (0x72, 4) => "vpsrad",
        (0x72, 6) => "vpslld",
        (0x73, 2) => "vpsrlq",
        (0x73, 6) => "vpsllq",
        _ => return Err(()),
    };
    let dst = vvvv_operand(v);
    let src = match mem {
        Some(mem) => Operand::Mem(mem),
        None => rm_operand(v, &m),
    };
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![dst, src, imm]))
}

/// 解码 VEX `0F 7E` 与 `0F 7F`。
fn vex_move_special(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    if op == 0x7F {
        return match v.pp {
            1 => v2_store(r, ctx, v, "vmovdqa", v.bits()),
            2 => v2_store(r, ctx, v, "vmovdqu", v.bits()),
            _ => Err(()),
        };
    }
    match v.pp {
        1 => {
            let bits = if v.w { 64 } else { 32 };
            let (m, mem) = read_rm(r, ctx, bits)?;
            let dst = match mem {
                Some(mem) => Operand::Mem(mem),
                None => Operand::Reg(Reg::gpr(bits, m.rm + if v.b { 8 } else { 0 }, false)),
            };
            let src = vreg(RegKind::Xmm, m.reg + if v.r { 8 } else { 0 });
            Ok(Decoded::new(if bits == 64 { "vmovq" } else { "vmovd" }, vec![dst, src]))
        }
        2 => v2(r, ctx, v, "vmovq", 64),
        _ => Err(()),
    }
}
