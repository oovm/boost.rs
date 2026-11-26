//! ModRM、SIB 与有效地址解码。

use super::{
    operand::{Mem, Reg},
    prefix::{Prefixes, Reader, address_size},
};

/// 已解码的 ModRM 字节。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ModRm {
    /// Mod 字段。
    pub(crate) mod_: u8,
    /// Reg 字段。
    pub(crate) reg: u8,
    /// Rm 字段。
    pub(crate) rm: u8,
}

impl ModRm {
    /// rm 字段是否选择寄存器。
    pub(crate) fn is_reg(&self) -> bool {
        self.mod_ == 3
    }
}

/// 构造通用寄存器操作数。
pub(crate) fn gpr(size: u32, num: u8, p: &Prefixes) -> Reg {
    Reg::gpr(size, num, p.has_rex())
}

/// 构造通用寄存器名。
fn gpr_name(size: u32, num: u8, p: &Prefixes) -> String {
    gpr(size, num, p).text()
}

/// 读取 ModRM 字节，并在其选择内存时读取有效地址。
pub(crate) fn read_modrm(r: &mut Reader<'_>, size: u32, bitness: u32, p: &Prefixes) -> Result<(ModRm, Option<Mem>), ()> {
    let byte = r.byte().ok_or(())?;
    let m = ModRm { mod_: byte >> 6, reg: (byte >> 3) & 7, rm: byte & 7 };
    if m.is_reg() {
        return Ok((m, None));
    }
    let mut mem = Mem::new(size);
    mem.seg = p.seg;
    let addr = address_size(bitness, p);
    if addr == 16 {
        decode16(r, &m, &mut mem)?;
    }
    else {
        decode32(r, &m, addr, p, &mut mem)?;
    }
    Ok((m, Some(mem)))
}

/// 按 `mod_` 隐含宽度读取位移写入 `mem`。
fn read_disp(r: &mut Reader<'_>, mod_: u8, mem: &mut Mem) -> Result<(), ()> {
    match mod_ {
        0 => {}
        1 => mem.disp = i64::from(r.byte().ok_or(())? as i8),
        2 => mem.disp = i64::from(r.u32().ok_or(())? as i32),
        _ => return Err(()),
    }
    Ok(())
}

/// 解码 32 位与 64 位寻址形式（含 SIB）。
fn decode32(r: &mut Reader<'_>, m: &ModRm, addr: u32, p: &Prefixes, mem: &mut Mem) -> Result<(), ()> {
    if m.rm == 4 {
        let sib = r.byte().ok_or(())?;
        let scale = sib >> 6;
        let index = (sib >> 3) & 7;
        let base = sib & 7;
        if index != 4 || p.x() {
            mem.index = Some(gpr_name(addr, index + if p.x() { 8 } else { 0 }, p));
            mem.scale = 1 << scale;
        }
        if base == 5 && m.mod_ == 0 {
            let disp = r.u32().ok_or(())?;
            if mem.index.is_some() {
                mem.disp = i64::from(disp as i32);
            }
            else {
                mem.disp = i64::from(disp);
                mem.absolute = true;
            }
        }
        else {
            mem.base = Some(gpr_name(addr, base + if p.b() { 8 } else { 0 }, p));
            read_disp(r, m.mod_, mem)?;
        }
        Ok(())
    }
    else if m.rm == 5 && m.mod_ == 0 {
        let disp = r.u32().ok_or(())?;
        if addr == 64 {
            mem.rip = true;
            mem.disp = i64::from(disp as i32);
        }
        else {
            mem.absolute = true;
            mem.disp = i64::from(disp);
        }
        Ok(())
    }
    else {
        mem.base = Some(gpr_name(addr, m.rm + if p.b() { 8 } else { 0 }, p));
        read_disp(r, m.mod_, mem)
    }
}

/// 解码 16 位寻址形式。
fn decode16(r: &mut Reader<'_>, m: &ModRm, mem: &mut Mem) -> Result<(), ()> {
    const BASES: [(&str, Option<&str>); 8] = [
        ("bx", Some("si")),
        ("bx", Some("di")),
        ("bp", Some("si")),
        ("bp", Some("di")),
        ("si", None),
        ("di", None),
        ("bp", None),
        ("bx", None),
    ];
    if m.mod_ == 0 && m.rm == 6 {
        mem.absolute = true;
        mem.disp = i64::from(r.u16().ok_or(())?);
        return Ok(());
    }
    let (base, index) = BASES[usize::from(m.rm)];
    mem.base = Some(base.to_string());
    mem.index = index.map(str::to_string);
    match m.mod_ {
        0 => {}
        1 => mem.disp = i64::from(r.byte().ok_or(())? as i8),
        2 => mem.disp = i64::from(r.u16().ok_or(())? as i16),
        _ => return Err(()),
    }
    Ok(())
}
