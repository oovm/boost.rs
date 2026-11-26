use crate::{ContainerError, Result};

use super::{
    MAX_BLOB_SIZE, PycCodeObject, PycConst, PycInstruction, RENDER_LIST_LIMIT, flags_text,
    render::{Tables, repr_value, resolve},
    render_list, split_localsplus,
};

/// A decoded `marshal` value, kept in memory only.
#[derive(Debug, Clone)]
pub(super) enum Value {
    None,
    Bool(bool),
    Int(i64),
    Long(String),
    Float(f64),
    Complex(String),
    Str(String),
    Bytes(Vec<u8>),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Set(Vec<Value>),
    Dict(Vec<(Value, Value)>),
    Code(usize),
    Ellipsis,
    StopIter,
    Unknown(String),
}

impl Value {
    /// Discriminator name used in [`PycConst::kind`].
    fn kind(&self) -> &'static str {
        match self {
            Value::None => "none",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Long(_) => "long",
            Value::Float(_) => "float",
            Value::Complex(_) => "complex",
            Value::Str(_) => "str",
            Value::Bytes(_) => "bytes",
            Value::Tuple(_) => "tuple",
            Value::List(_) => "list",
            Value::Set(_) => "set",
            Value::Dict(_) => "dict",
            Value::Code(_) => "code",
            Value::Ellipsis => "ellipsis",
            Value::StopIter => "stopiter",
            Value::Unknown(_) => "unknown",
        }
    }
}

/// Parses one `marshal` v4 stream into flattened code objects.
pub(super) struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    pub(super) version: Option<u8>,
    python: u16,
    opcodes: &'static [(u8, &'static str)],
    refs: Vec<Value>,
    pub(super) codes: Vec<PycCodeObject>,
    pub(super) decoded_codes: usize,
    budget: usize,
    pub(super) truncated: bool,
    code_limit: usize,
}

impl<'a> Parser<'a> {
    /// Creates a parser over one stream body, disassembling with the opcode
    /// table of `python`.
    pub(super) fn new(
        data: &'a [u8],
        code_limit: usize,
        instruction_limit: usize,
        python: u16,
        opcodes: &'static [(u8, &'static str)],
    ) -> Self {
        Self {
            data,
            pos: 0,
            version: None,
            python,
            opcodes,
            refs: Vec::new(),
            codes: Vec::new(),
            decoded_codes: 0,
            budget: instruction_limit,
            truncated: false,
            code_limit,
        }
    }

    /// Reads the optional version byte and the single top-level object.
    pub(super) fn stream(&mut self) -> Result<Value> {
        if let Some(&version @ 2..=6) = self.data.get(self.pos) {
            self.version = Some(version);
            self.pos += 1;
        }
        self.object()
    }

    /// Reads one raw byte.
    fn byte(&mut self) -> Result<u8> {
        let value = *self.data.get(self.pos).ok_or_else(|| self.truncated_error())?;
        self.pos += 1;
        Ok(value)
    }

    /// Reads a little-endian word.
    fn i32(&mut self) -> Result<i32> {
        let end = self.pos.checked_add(4).ok_or_else(|| self.truncated_error())?;
        let window: [u8; 4] = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| self.truncated_error())?
            .try_into()
            .expect("four-byte window");
        self.pos = end;
        Ok(i32::from_le_bytes(window))
    }

    /// Reads a little-endian double word.
    fn i64(&mut self) -> Result<i64> {
        let end = self.pos.checked_add(8).ok_or_else(|| self.truncated_error())?;
        let window: [u8; 8] = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| self.truncated_error())?
            .try_into()
            .expect("eight-byte window");
        self.pos = end;
        Ok(i64::from_le_bytes(window))
    }

    /// Reads a little-endian half word.
    fn u16(&mut self) -> Result<u16> {
        let end = self.pos.checked_add(2).ok_or_else(|| self.truncated_error())?;
        let window: [u8; 2] = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| self.truncated_error())?
            .try_into()
            .expect("two-byte window");
        self.pos = end;
        Ok(u16::from_le_bytes(window))
    }

    /// Error for a stream that ends mid-object.
    fn truncated_error(&self) -> ContainerError {
        ContainerError::parse(format!("the marshal stream ends at offset {}", self.pos))
            .at(self.pos as u64, self.data.len() as u64)
    }

    /// Reads one object, honouring the reference flag and interning.
    fn object(&mut self) -> Result<Value> {
        let byte = self.byte()?;
        let flagged = byte & 0x80 != 0;
        let kind = byte & 0x7f;
        // `marshal` reserves the reference slot when the type byte is seen,
        // before the object body is written, so nested references number the
        // parent first (pre-order). Interned string kinds join the table even
        // without the flag bit. An empty `Unknown` marks a slot whose object
        // is still being parsed.
        let slot = if flagged || matches!(kind, b't' | b'A' | b'Z') {
            let index = self.refs.len();
            self.refs.push(Value::Unknown(String::new()));
            Some(index)
        }
        else {
            None
        };
        let value = match kind {
            b'0' | b'N' => Value::None,
            b'F' => Value::Bool(false),
            b'T' => Value::Bool(true),
            b'S' => Value::StopIter,
            b'.' => Value::Ellipsis,
            b'i' => Value::Int(i64::from(self.i32()?)),
            b'I' => Value::Int(self.i64()?),
            b'b' => Value::Int(i64::from(self.byte()? as i8)),
            b'l' => self.long()?,
            b'g' => Value::Float(f64::from_bits(self.i64()? as u64)),
            b'f' => Value::Float(f64::from(f32::from_bits(self.i32()? as u32))),
            b'x' => {
                let real = f64::from_bits(self.i64()? as u64);
                let imag = f64::from_bits(self.i64()? as u64);
                Value::Complex(format!("({real}+{imag}j)"))
            }
            b's' => Value::Bytes(self.blob()?),
            b'a' | b'A' | b't'..=b'u' => Value::Str(self.text()?),
            b'z' | b'Z' => {
                let len = usize::from(self.byte()?);
                let end = self.pos.checked_add(len).ok_or_else(|| self.truncated_error())?;
                let bytes = self.data.get(self.pos..end).ok_or_else(|| self.truncated_error())?.to_vec();
                self.pos = end;
                Value::Str(String::from_utf8_lossy(&bytes).into_owned())
            }
            b'r' => {
                let index = self.i32()?;
                match usize::try_from(index).ok().and_then(|i| self.refs.get(i)).cloned() {
                    Some(Value::Unknown(text)) if text.is_empty() => {
                        return Err(ContainerError::parse(format!(
                            "marshal reference {index} points at an object that is not complete"
                        )));
                    }
                    Some(value) => value,
                    None => Value::Unknown(format!("ref {index}")),
                }
            }
            b'(' => Value::Tuple(self.seq_of()?),
            b'[' => Value::List(self.seq_of()?),
            b'<' => Value::Set(self.seq_of()?),
            b'>' => Value::Set(self.seq_of()?),
            b')' => {
                let count = i32::from(self.byte()?);
                Value::Tuple(self.seq(count)?)
            }
            b'{' => self.dict()?,
            b'c' => self.code()?,
            other => {
                let start = self.pos as u64 - 1;
                return Err(ContainerError::parse(format!(
                    "unsupported marshal type '{}' at stream offset {}",
                    other as char,
                    self.pos - 1
                ))
                .at(start, start + 1));
            }
        };
        if let Some(index) = slot {
            self.refs[index] = value.clone();
        }
        Ok(value)
    }

    /// Reads a length word and the sequence it counts.
    fn seq_of(&mut self) -> Result<Vec<Value>> {
        let count = self.i32()?;
        self.seq(count)
    }

    /// Reads a sequence of `count` objects, guarding the count.
    fn seq(&mut self, count: i32) -> Result<Vec<Value>> {
        let count = usize::try_from(count).map_err(|_| self.count_error())?;
        if count > 1 << 22 {
            return Err(self.count_error());
        }
        let mut items = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            items.push(self.object()?);
        }
        Ok(items)
    }

    /// Error for an implausible element count.
    fn count_error(&self) -> ContainerError {
        ContainerError::parse(format!("implausible marshal element count at offset {}", self.pos))
            .at(self.pos as u64, self.data.len() as u64)
    }

    /// Reads key/value pairs until the `null` terminator.
    fn dict(&mut self) -> Result<Value> {
        let mut pairs = Vec::new();
        loop {
            let peek = *self.data.get(self.pos).ok_or_else(|| self.truncated_error())?;
            if peek & 0x7f == b'0' {
                self.pos += 1;
                break;
            }
            let key = self.object()?;
            let value = self.object()?;
            pairs.push((key, value));
            if pairs.len() > 1 << 22 {
                return Err(self.count_error());
            }
        }
        Ok(Value::Dict(pairs))
    }

    /// Reads a `TYPE_LONG`: digit count, sign, base-2^15 digits.
    fn long(&mut self) -> Result<Value> {
        let raw = self.i32()?;
        let negative = raw < 0;
        let digits = raw.unsigned_abs() as usize;
        if digits > 65_536 {
            return Err(self.count_error());
        }
        let mut acc: u128 = 0;
        let mut overflow = false;
        for index in 0..digits {
            let digit = u128::from(self.u16()?);
            if !overflow {
                if index >= 8 {
                    overflow = true;
                }
                else {
                    acc += digit << (15 * index);
                }
            }
        }
        if overflow {
            return Ok(Value::Long(format!("<long {digits} digits>")));
        }
        Ok(Value::Long(if negative { format!("-{acc}") } else { acc.to_string() }))
    }

    /// Reads a length-prefixed byte blob.
    fn blob(&mut self) -> Result<Vec<u8>> {
        let len = self.i32()?;
        let start = self.pos as u64;
        let len = usize::try_from(len).map_err(|_| {
            ContainerError::parse("negative marshal blob length").at(start, self.data.len() as u64)
        })?;
        if len > MAX_BLOB_SIZE {
            return Err(ContainerError::budget(format!("marshal blob of {len} bytes is over the cap"))
                .at(start, start + len as u64));
        }
        let end = self.pos.checked_add(len).ok_or_else(|| self.truncated_error())?;
        let bytes = self.data.get(self.pos..end).ok_or_else(|| self.truncated_error())?.to_vec();
        self.pos = end;
        Ok(bytes)
    }

    /// Reads a length-prefixed UTF-8 string.
    fn text(&mut self) -> Result<String> {
        let bytes = self.blob()?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Reads one `code` object and disassembles it immediately.
    fn code(&mut self) -> Result<Value> {
        if self.codes.len() >= self.code_limit {
            self.truncated = true;
            return Err(ContainerError::parse(format!("the marshal stream holds more than {} code objects", self.code_limit))
                .at(self.pos as u64, self.data.len() as u64));
        }
        // The scalar code fields are raw little-endian words, not objects.
        let argcount = self.i32()?;
        let posonlyargcount = self.i32()?;
        let kwonlyargcount = self.i32()?;
        let stacksize = self.i32()?;
        let flags = self.i32()? as u32;
        let code = match self.object()? {
            Value::Bytes(bytes) => bytes,
            other => return Err(ContainerError::parse(format!("code field is a marshal {}, not bytes", other.kind()))),
        };
        let consts = match self.object()? {
            Value::Tuple(items) => items,
            other => return Err(ContainerError::parse(format!("consts field is a marshal {}, not a tuple", other.kind()))),
        };
        let names = self.string_tuple("names")?;
        let localsplus = self.string_tuple("localsplusnames")?;
        let kinds = match self.object()? {
            Value::Bytes(bytes) => bytes,
            other => return Err(ContainerError::parse(format!("localspluskinds field is a marshal {}, not bytes", other.kind()))),
        };
        let (varnames, freevars, cellvars) = split_localsplus(&localsplus, &kinds);
        let filename = self.expect_str("filename")?;
        let name = self.expect_str("name")?;
        let qualname = self.expect_str("qualname")?;
        let firstlineno = self.i32()?;
        let linetable = match self.object()? {
            Value::Bytes(bytes) => bytes,
            other => return Err(ContainerError::parse(format!("linetable field is a marshal {}, not bytes", other.kind()))),
        };
        let exceptiontable = match self.object()? {
            Value::Bytes(bytes) => bytes,
            other => return Err(ContainerError::parse(format!("exceptiontable field is a marshal {}, not bytes", other.kind()))),
        };

        let mut truncated = false;
        let mut rendered_consts = Vec::new();
        for item in consts.iter().take(RENDER_LIST_LIMIT) {
            let code_index = match item {
                Value::Code(index) => Some(*index),
                _ => None,
            };
            rendered_consts.push(PycConst { kind: item.kind().to_string(), repr: repr_value(item, &self.codes), code_index });
        }
        if consts.len() > RENDER_LIST_LIMIT {
            truncated = true;
        }

        let mut cells = cellvars.clone();
        cells.extend(freevars.iter().cloned());
        let (instructions, cut) = self.disassemble(&code, &names, &consts, &varnames, &cells);
        if cut {
            truncated = true;
        }

        let index = self.codes.len();
        self.decoded_codes += 1;
        self.codes.push(PycCodeObject {
            index,
            qualname,
            name,
            filename,
            argcount,
            posonlyargcount,
            kwonlyargcount,
            stacksize,
            flags,
            flags_text: flags_text(flags),
            firstlineno,
            code_len: code.len() as u32,
            linetable_len: linetable.len() as u32,
            exceptiontable_len: exceptiontable.len() as u32,
            names: render_list(names, &mut truncated),
            varnames: render_list(varnames, &mut truncated),
            freevars: render_list(freevars, &mut truncated),
            cellvars: render_list(cellvars, &mut truncated),
            consts: rendered_consts,
            instructions,
            truncated,
        });
        Ok(Value::Code(index))
    }

    /// Reads a tuple-of-strings field such as `co_names`.
    fn string_tuple(&mut self, field: &str) -> Result<Vec<String>> {
        let items = match self.object()? {
            Value::Tuple(items) => items,
            other => return Err(ContainerError::parse(format!("{field} field is a marshal {}, not a tuple", other.kind()))),
        };
        items
            .iter()
            .map(|item| match item {
                Value::Str(text) => Ok(text.clone()),
                other => Err(ContainerError::parse(format!("{field} holds a marshal {}, not a string", other.kind()))),
            })
            .collect()
    }

    /// Reads a string field of a code object.
    fn expect_str(&mut self, field: &str) -> Result<String> {
        match self.object()? {
            Value::Str(text) => Ok(text),
            other => Err(ContainerError::parse(format!("{field} field is a marshal {}, not a string", other.kind()))),
        }
    }

    /// Walks `co_code` as word-code pairs and resolves arguments.
    ///
    /// The walk emits the instructions first, counting the inline cache
    /// slots against them, and resolves the arguments in a second pass:
    /// from 3.12 on, a relative jump target is computed past the caches of
    /// the jumping instruction, exactly like `dis` renders it.
    fn disassemble(
        &mut self,
        code: &[u8],
        names: &[String],
        consts: &[Value],
        varnames: &[String],
        cells: &[String],
    ) -> (Vec<PycInstruction>, bool) {
        let tables = Tables { names, consts, varnames, cells, codes: &self.codes };
        let extended_arg = crate::pyc_opcodes::extended_arg(self.python);
        let mut out: Vec<PycInstruction> = Vec::new();
        let mut cut = false;
        let mut extended: u32 = 0;
        let mut offset = 0usize;
        while offset + 1 < code.len() {
            let op = code[offset];
            let byte = code[offset + 1];
            if op == 0 {
                if let Some(last) = out.last_mut() {
                    last.caches += 1;
                }
                offset += 2;
                continue;
            }
            let arg =
                if crate::pyc_opcodes::has_argument(self.python, op) { Some(u32::from(byte) | extended) } else { None };
            extended = if op == extended_arg { (u32::from(byte) | extended) << 8 } else { 0 };
            if self.budget == 0 {
                cut = true;
                offset += 2;
                continue;
            }
            self.budget -= 1;
            let name = match crate::pyc_opcodes::lookup(self.opcodes, op) {
                Some(name) => name.to_string(),
                None => format!("<opcode {op}>"),
            };
            out.push(PycInstruction { offset: offset as u32, opcode: op, name, arg, resolved: None, caches: 0 });
            offset += 2;
        }
        for instruction in &mut out {
            if let Some(arg) = instruction.arg {
                instruction.resolved =
                    resolve(&instruction.name, self.python, arg, instruction.offset, instruction.caches, &tables);
            }
        }
        (out, cut)
    }
}
