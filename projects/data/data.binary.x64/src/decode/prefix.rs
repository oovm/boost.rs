//! 旧式前缀、REX 与字节游标处理。

/// 指令编码字节上的游标。
pub(crate) struct Reader<'a> {
    /// 整个代码块的编码字节。
    pub bytes: &'a [u8],
    /// 下一个未读字节的偏移。
    pub pos: usize,
}

impl<'a> Reader<'a> {
    /// 在 `pos` 处创建游标。
    pub(crate) fn new(bytes: &'a [u8], pos: usize) -> Self {
        Self { bytes, pos }
    }

    /// 返回下一个字节但不消耗。
    pub(crate) fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// 消耗并返回下一个字节。
    pub(crate) fn byte(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    /// 消耗一个小端 `u16`。
    pub(crate) fn u16(&mut self) -> Option<u16> {
        let low = u16::from(self.byte()?);
        let high = u16::from(self.byte()?);
        Some(low | high << 8)
    }

    /// 消耗一个小端 `u32`。
    pub(crate) fn u32(&mut self) -> Option<u32> {
        let low = u32::from(self.u16()?);
        let high = u32::from(self.u16()?);
        Some(low | high << 16)
    }

    /// 消耗一个小端 `u64`。
    pub(crate) fn u64(&mut self) -> Option<u64> {
        let low = u64::from(self.u32()?);
        let high = u64::from(self.u32()?);
        Some(low | high << 32)
    }
}

/// 已解码的旧式前缀与 REX 状态。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Prefixes {
    /// REX 字节，缺省时为 `0`。
    pub(crate) rex: u8,
    /// 操作数宽度覆盖（`0x66`）。
    pub(crate) opsize: bool,
    /// 地址宽度覆盖（`0x67`）。
    pub(crate) addrsize: bool,
    /// 重复前缀（`0xF2` 或 `0xF3`）。
    pub(crate) rep: Option<u8>,
    /// 锁前缀（`0xF0`）。
    pub(crate) lock: bool,
    /// 段覆盖助记符。
    pub(crate) seg: Option<&'static str>,
}

impl Prefixes {
    /// 是否存在任意 REX 前缀。
    pub(crate) fn has_rex(&self) -> bool {
        self.rex != 0
    }

    /// REX.W 位。
    pub(crate) fn w(&self) -> bool {
        self.rex & 0x08 != 0
    }

    /// REX.R 位。
    pub(crate) fn r(&self) -> bool {
        self.rex & 0x04 != 0
    }

    /// REX.X 位。
    pub(crate) fn x(&self) -> bool {
        self.rex & 0x02 != 0
    }

    /// REX.B 位。
    pub(crate) fn b(&self) -> bool {
        self.rex & 0x01 != 0
    }
}

/// 消耗全部前导前缀。
pub(crate) fn read_prefixes(r: &mut Reader<'_>, bitness: u32) -> Prefixes {
    let mut p = Prefixes::default();
    loop {
        let Some(byte) = r.peek()
        else {
            break;
        };
        match byte {
            0x66 => {
                p.opsize = true;
                p.rex = 0;
            }
            0x67 => {
                p.addrsize = true;
                p.rex = 0;
            }
            0xF0 => {
                p.lock = true;
                p.rex = 0;
            }
            0xF2 | 0xF3 => {
                p.rep = Some(byte);
                p.rex = 0;
            }
            0x2E => {
                p.seg = Some("cs");
                p.rex = 0;
            }
            0x36 => {
                p.seg = Some("ss");
                p.rex = 0;
            }
            0x3E => {
                p.seg = Some("ds");
                p.rex = 0;
            }
            0x26 => {
                p.seg = Some("es");
                p.rex = 0;
            }
            0x64 => {
                p.seg = Some("fs");
                p.rex = 0;
            }
            0x65 => {
                p.seg = Some("gs");
                p.rex = 0;
            }
            0x40..=0x4F if bitness == 64 => {
                p.rex = byte;
            }
            _ => break,
        }
        r.pos += 1;
    }
    p
}

/// 指令的有效操作数宽度，不含逐操作码覆盖。
pub(crate) fn operand_size(bitness: u32, p: &Prefixes) -> u32 {
    if bitness == 64 {
        if p.w() {
            64
        }
        else if p.opsize {
            16
        }
        else {
            32
        }
    }
    else if p.opsize {
        16
    }
    else {
        32
    }
}

/// 指令的有效地址宽度。
pub(crate) fn address_size(bitness: u32, p: &Prefixes) -> u32 {
    if bitness == 64 {
        if p.addrsize { 32 } else { 64 }
    }
    else if p.addrsize {
        16
    }
    else {
        32
    }
}
