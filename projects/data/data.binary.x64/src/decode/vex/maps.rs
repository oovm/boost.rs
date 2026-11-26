//! VEX `0F 38` / `0F 3A` map 与 EVEX `0F` map。

use super::{Vex, read_rm, reg_operand, rm_operand, scalar_name, v2, v2_imm, v2_store, v3, v3_imm, vreg};
use crate::decode::{
    Decoded,
    opcode::{Ctx, imm_operand},
    operand::{Operand, Reg, RegKind},
    prefix::Reader,
};

/// 解码 VEX `0F 38` map。
pub(super) fn map2(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let bits = v.bits();
    match op {
        0x00..=0x0B => {
            if v.pp != 1 {
                return Err(());
            }
            let name = match op {
                0x00 => "vpshufb",
                0x01 => "vphaddw",
                0x02 => "vphaddd",
                0x03 => "vphaddsw",
                0x04 => "vpmaddubsw",
                0x05 => "vphsubw",
                0x06 => "vphsubd",
                0x07 => "vphsubsw",
                0x08 => "vpsignb",
                0x09 => "vpsignw",
                0x0A => "vpsignd",
                _ => "vpmulhrsw",
            };
            v3(r, ctx, v, name, bits)
        }
        0x0C => v3(r, ctx, v, "vpermilps", bits),
        0x0D => v3(r, ctx, v, "vpermilpd", bits),
        0x17 => v2(r, ctx, v, "vptest", bits),
        0x18 => v2(r, ctx, v, "vbroadcastss", 32),
        0x19 => v2(r, ctx, v, "vbroadcastsd", 64),
        0x1A => v2(r, ctx, v, "vbroadcastf128", 128),
        0x1C..=0x1E => {
            let name = match op {
                0x1C => "vpabsb",
                0x1D => "vpabsw",
                _ => "vpabsd",
            };
            v2(r, ctx, v, name, bits)
        }
        0x20..=0x25 => vpmovx_vex(r, ctx, v, op, true),
        0x28 => v3(r, ctx, v, "vpmuldq", bits),
        0x29 => v3(r, ctx, v, "vpcmpeqq", bits),
        0x2A => v2(r, ctx, v, "vmovntdqa", bits),
        0x2B => v3(r, ctx, v, "vpackusdw", bits),
        0x30..=0x35 => vpmovx_vex(r, ctx, v, op, false),
        0x37 => v3(r, ctx, v, "vpcmpgtq", bits),
        0x38..=0x3F => {
            let name = match op {
                0x38 => "vpminsb",
                0x39 => "vpminsd",
                0x3A => "vpminuw",
                0x3B => "vpminud",
                0x3C => "vpmaxsb",
                0x3D => "vpmaxsd",
                0x3E => "vpmaxuw",
                _ => "vpmaxud",
            };
            v3(r, ctx, v, name, bits)
        }
        0x40 => v3(r, ctx, v, "vpmulld", bits),
        0x41 => v2(r, ctx, v, "vphminposuw", bits),
        0xDB..=0xDF => {
            let name = match op {
                0xDB => "vaesimc",
                0xDC => "vaesenc",
                0xDD => "vaesenclast",
                0xDE => "vaesdec",
                _ => "vaesdeclast",
            };
            if op == 0xDB { v2(r, ctx, v, name, 128) } else { v3(r, ctx, v, name, 128) }
        }
        _ => Err(()),
    }
}

/// 解码 VEX 加宽移动（`vpmovsx*` 或 `vpmovzx*`）。
fn vpmovx_vex(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8, signed: bool) -> Result<Decoded, ()> {
    if v.pp != 1 {
        return Err(());
    }
    let name = match (op & 7, signed) {
        (0, true) => "vpmovsxbw",
        (1, true) => "vpmovsxbd",
        (2, true) => "vpmovsxbq",
        (3, true) => "vpmovsxwd",
        (4, true) => "vpmovsxwq",
        (5, true) => "vpmovsxdq",
        (0, false) => "vpmovzxbw",
        (1, false) => "vpmovzxbd",
        (2, false) => "vpmovzxbq",
        (3, false) => "vpmovzxwd",
        (4, false) => "vpmovzxwq",
        _ => "vpmovzxdq",
    };
    let bits = match op & 7 {
        2 => 16,
        1 | 4 => 32,
        _ => 64,
    };
    v2(r, ctx, v, name, bits)
}

/// 解码 VEX `0F 3A` map。
pub(super) fn map3(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let bits = v.bits();
    match op {
        0x08 => v2_imm(r, ctx, v, "vroundps", bits),
        0x09 => v2_imm(r, ctx, v, "vroundpd", bits),
        0x0A => v3_imm(r, ctx, v, "vroundss", 32),
        0x0B => v3_imm(r, ctx, v, "vroundsd", 64),
        0x0C => v3_imm(r, ctx, v, "vblendps", bits),
        0x0D => v3_imm(r, ctx, v, "vblendpd", bits),
        0x0E => v3_imm(r, ctx, v, "vpblendw", bits),
        0x0F => v3_imm(r, ctx, v, "vpalignr", bits),
        0x14..=0x17 => vex_extract(r, ctx, v, op),
        0x20..=0x22 => vex_insert(r, ctx, v, op),
        0x38 => v3_imm(r, ctx, v, "vinserti128", bits),
        0x39 => {
            let (m, mem) = read_rm(r, ctx, bits)?;
            let src = match mem {
                Some(mem) => Operand::Mem(mem),
                None => rm_operand(v, &m),
            };
            let dst = reg_operand(v, &m);
            let imm = imm_operand(r, 8, 8, false)?;
            Ok(Decoded::new("vextracti128", vec![src, dst, imm]))
        }
        0x40 => v3_imm(r, ctx, v, "vdpps", bits),
        0x41 => v3_imm(r, ctx, v, "vdppd", bits),
        0x42 => v3_imm(r, ctx, v, "vmpsadbw", bits),
        0x44 => v3_imm(r, ctx, v, "vpclmulqdq", bits),
        0xDF => v2_imm(r, ctx, v, "vaeskeygenassist", 128),
        _ => Err(()),
    }
}

/// 解码 VEX 提取指令。
fn vex_extract(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let bits = match op {
        0x14 => 8,
        0x15 => 16,
        0x16 => {
            if v.w {
                64
            }
            else {
                32
            }
        }
        _ => 32,
    };
    let name = match op {
        0x14 => "vpextrb",
        0x15 => "vpextrw",
        0x16 => {
            if bits == 64 {
                "vpextrq"
            }
            else {
                "vpextrd"
            }
        }
        _ => "vextractps",
    };
    let (m, mem) = read_rm(r, ctx, bits)?;
    let dst = match mem {
        Some(mem) => Operand::Mem(mem),
        None => Operand::Reg(Reg::gpr(bits.max(32), m.rm + if v.b { 8 } else { 0 }, false)),
    };
    let src = vreg(RegKind::Xmm, m.reg + if v.r { 8 } else { 0 });
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![dst, src, imm]))
}

/// 解码 VEX 插入指令。
fn vex_insert(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let bits = match op {
        0x20 => 8,
        0x21 => 32,
        _ => {
            if v.w {
                64
            }
            else {
                32
            }
        }
    };
    let name = match op {
        0x20 => "vpinsrb",
        0x21 => "vinsertps",
        _ => {
            if bits == 64 {
                "vpinsrq"
            }
            else {
                "vpinsrd"
            }
        }
    };
    let (m, mem) = read_rm(r, ctx, bits)?;
    let dst = reg_operand(v, &m);
    let src2 = vreg(RegKind::Xmm, v.vvvv);
    let src3 = match mem {
        Some(mem) => Operand::Mem(mem),
        None if op == 0x21 => vreg(RegKind::Xmm, m.rm + if v.b { 8 } else { 0 }),
        None => Operand::Reg(Reg::gpr(bits.max(32), m.rm + if v.b { 8 } else { 0 }, false)),
    };
    let imm = imm_operand(r, 8, 8, false)?;
    Ok(Decoded::new(name, vec![dst, src2, src3, imm]))
}

/// 解码 EVEX `0F` map 子集。
pub(super) fn evex_map1(r: &mut Reader<'_>, ctx: &Ctx, v: &Vex, op: u8) -> Result<Decoded, ()> {
    let bits = v.bits();
    match op {
        0x10 | 0x11 => {
            let name = match v.pp {
                0 => "vmovups",
                1 => "vmovupd",
                _ => return Err(()),
            };
            if op == 0x10 { v2(r, ctx, v, name, bits) } else { v2_store(r, ctx, v, name, bits) }
        }
        0x28 | 0x29 => {
            let name = match v.pp {
                0 => "vmovaps",
                1 => "vmovapd",
                _ => return Err(()),
            };
            if op == 0x28 { v2(r, ctx, v, name, bits) } else { v2_store(r, ctx, v, name, bits) }
        }
        0x51 => {
            let name = match v.pp {
                0 => "vsqrtps",
                1 => "vsqrtpd",
                _ => return Err(()),
            };
            v2(r, ctx, v, name, bits)
        }
        0x54..=0x57 => {
            let name = match (op, v.pp) {
                (0x54, 0) => "vandps",
                (0x54, 1) => "vandpd",
                (0x55, 0) => "vandnps",
                (0x55, 1) => "vandnpd",
                (0x56, 0) => "vorps",
                (0x56, 1) => "vorpd",
                (0x57, 0) => "vxorps",
                (0x57, 1) => "vxorpd",
                _ => return Err(()),
            };
            v3(r, ctx, v, name, bits)
        }
        0x58 | 0x59 | 0x5C..=0x5F => {
            let Some((name, scalar_bits)) = scalar_name(op, v.pp)
            else {
                return Err(());
            };
            v3(r, ctx, v, name, if scalar_bits == 32 || scalar_bits == 64 { scalar_bits } else { bits })
        }
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
        0x6F | 0x7F => {
            let name = match (v.pp, v.w) {
                (1, false) => "vmovdqa32",
                (1, true) => "vmovdqa64",
                (2, false) => "vmovdqu32",
                (2, true) => "vmovdqu64",
                _ => return Err(()),
            };
            if op == 0x6F { v2(r, ctx, v, name, bits) } else { v2_store(r, ctx, v, name, bits) }
        }
        0x70 => v2_imm(r, ctx, v, "vpshufd", bits),
        0x74..=0x76 => {
            let name = match op {
                0x74 => "vpcmpeqb",
                0x75 => "vpcmpeqw",
                _ => "vpcmpeqd",
            };
            v3(r, ctx, v, name, bits)
        }
        0xDB => v3(r, ctx, v, if v.w { "vpandq" } else { "vpandd" }, bits),
        0xDF => v3(r, ctx, v, if v.w { "vpandnq" } else { "vpandnd" }, bits),
        0xEB => v3(r, ctx, v, if v.w { "vporq" } else { "vpord" }, bits),
        0xEF => v3(r, ctx, v, if v.w { "vpxorq" } else { "vpxord" }, bits),
        0xF8 => v3(r, ctx, v, "vpsubb", bits),
        0xF9 => v3(r, ctx, v, "vpsubw", bits),
        0xFA => v3(r, ctx, v, if v.w { "vpsubq" } else { "vpsubd" }, bits),
        0xFB => v3(r, ctx, v, "vpsubq", bits),
        0xFC => v3(r, ctx, v, "vpaddb", bits),
        0xFD => v3(r, ctx, v, "vpaddw", bits),
        0xFE => v3(r, ctx, v, if v.w { "vpaddq" } else { "vpaddd" }, bits),
        _ => Err(()),
    }
}
