use super::{PycCodeObject, marshal::Value};

/// The lookup tables one code object resolves its arguments against.
pub(super) struct Tables<'a> {
    pub(super) names: &'a [String],
    pub(super) consts: &'a [Value],
    pub(super) varnames: &'a [String],
    pub(super) cells: &'a [String],
    pub(super) codes: &'a [PycCodeObject],
}

/// Resolves one instruction argument into display text, mirroring the rule
/// the `dis` module of the selected interpreter applies to the opcode name.
pub(super) fn resolve(name: &str, python: u16, arg: u32, offset: u32, caches: u32, tables: &Tables) -> Option<String> {
    let Tables { names, consts, varnames, cells, codes } = *tables;
    let index = arg as usize;
    match name {
        "LOAD_CONST" | "RETURN_CONST" | "KW_NAMES" | "CALL_KW" => consts.get(index).map(|value| repr_value(value, codes)),
        "LOAD_ATTR" if python != 311 => {
            let base = names.get(index / 2)?;
            Some(if arg & 1 == 1 { format!("NULL|self + {base}") } else { base.clone() })
        }
        "LOAD_GLOBAL" => {
            let base = names.get(index / 2)?;
            Some(if arg & 1 == 1 {
                if python == 313 { format!("{base} + NULL") } else { format!("NULL + {base}") }
            }
            else {
                base.clone()
            })
        }
        "LOAD_SUPER_ATTR" => {
            let base = names.get(index / 4)?;
            Some(if arg & 1 == 1 { format!("NULL|self + {base}") } else { base.clone() })
        }
        "STORE_NAME"
        | "DELETE_NAME"
        | "STORE_ATTR"
        | "DELETE_ATTR"
        | "STORE_GLOBAL"
        | "DELETE_GLOBAL"
        | "LOAD_NAME"
        | "LOAD_ATTR"
        | "IMPORT_NAME"
        | "IMPORT_FROM"
        | "LOAD_METHOD"
        | "LOAD_FROM_DICT_OR_GLOBALS" => names.get(index).cloned(),
        "LOAD_FAST" | "STORE_FAST" | "DELETE_FAST" | "LOAD_FAST_CHECK" | "LOAD_FAST_AND_CLEAR" | "STORE_FAST_MAYBE_NULL" => {
            varnames.get(index).cloned()
        }
        "LOAD_FAST_LOAD_FAST" | "STORE_FAST_LOAD_FAST" | "STORE_FAST_STORE_FAST" => {
            let first = varnames.get((index >> 4) & 0xf)?;
            let second = varnames.get(index & 0xf)?;
            Some(format!("{first}, {second}"))
        }
        // From 3.11 the free-variable opcodes index `co_localsplusnames`:
        // locals first, then cells and frees, so the name lives in `varnames`
        // below their count and in `cells` above it.
        "MAKE_CELL"
        | "LOAD_CLOSURE"
        | "LOAD_DEREF"
        | "STORE_DEREF"
        | "DELETE_DEREF"
        | "LOAD_CLASSDEREF"
        | "LOAD_FROM_DICT_OR_DEREF" => {
            if index < varnames.len() {
                varnames.get(index).cloned()
            }
            else {
                cells.get(index - varnames.len()).cloned()
            }
        }
        "IS_OP" => Some(if arg & 1 == 1 { "is not".to_string() } else { "is".to_string() }),
        "CONTAINS_OP" => Some(if arg & 1 == 1 { "not in".to_string() } else { "in".to_string() }),
        "COMPARE_OP" => {
            // The operator index moved into the high bits of the argument:
            // raw in 3.11, above bit 4 in 3.12 and above bit 5 in 3.13,
            // where bit 4 marks the 3.13 boolean coercion.
            let (shift, operators): (u32, &[&str]) = match python {
                311 => (0, &CMP_OPS_311),
                312 => (4, &CMP_OPS),
                _ => (5, &CMP_OPS),
            };
            let text = operators.get(index >> shift as usize)?;
            Some(if python == 313 && arg & 16 != 0 { format!("bool({text})") } else { (*text).to_string() })
        }
        "MAKE_FUNCTION" | "SET_FUNCTION_ATTRIBUTE" => {
            let mut parts = Vec::new();
            for (bit, name) in [(1, "defaults"), (2, "kwdefaults"), (4, "annotations"), (8, "closure")] {
                if arg & bit != 0 {
                    parts.push(name);
                }
            }
            Some(parts.join(", "))
        }
        "FORMAT_VALUE" => {
            let converter = ["", "str", "repr", "ascii"][usize::try_from(arg & 3).unwrap_or(0)];
            let format = if arg & 4 != 0 { ", with format" } else { "" };
            Some(format!("{}{format}", if converter.is_empty() { "none" } else { converter }))
        }
        "BINARY_OP" => NB_OPS.get(index).map(|text| (*text).to_string()),
        "CALL_INTRINSIC_1" => INTRINSIC_1.get(index).map(|text| (*text).to_string()),
        "CALL_INTRINSIC_2" => INTRINSIC_2.get(index).map(|text| (*text).to_string()),
        "CONVERT_VALUE" => ["", "str", "repr", "ascii"].get(index).map(|text| (*text).to_string()),
        _ if is_jump(name) => {
            // Relative jumps: forward unless the name says backward, the same
            // rule `dis` applies; 3.13 additionally encodes backward
            // conditional jumps as a negative argument. From 3.12 the target
            // skips the cache slots of the jumping instruction.
            let signed: i64 = if name.contains("JUMP_BACKWARD") {
                -i64::from(arg)
            }
            else if python == 313 {
                (arg as i32) as i64
            }
            else {
                i64::from(arg)
            };
            let mut target = i64::from(offset) + 2 + signed * 2;
            if python >= 312 {
                target += i64::from(caches) * 2;
            }
            Some(format!("to {target}"))
        }
        _ => None,
    }
}

/// Whether one opcode name is a relative jump.
fn is_jump(name: &str) -> bool {
    matches!(
        name,
        "FOR_ITER"
            | "SEND"
            | "JUMP_FORWARD"
            | "JUMP_BACKWARD"
            | "JUMP_BACKWARD_NO_INTERRUPT"
            | "JUMP_IF_FALSE_OR_POP"
            | "JUMP_IF_TRUE_OR_POP"
            | "POP_JUMP_IF_FALSE"
            | "POP_JUMP_IF_TRUE"
            | "POP_JUMP_IF_NONE"
            | "POP_JUMP_IF_NOT_NONE"
            | "POP_JUMP_FORWARD_IF_FALSE"
            | "POP_JUMP_FORWARD_IF_TRUE"
            | "POP_JUMP_FORWARD_IF_NONE"
            | "POP_JUMP_FORWARD_IF_NOT_NONE"
            | "POP_JUMP_BACKWARD_IF_FALSE"
            | "POP_JUMP_BACKWARD_IF_TRUE"
            | "POP_JUMP_BACKWARD_IF_NONE"
            | "POP_JUMP_BACKWARD_IF_NOT_NONE"
    )
}

/// The six comparison operators of 3.12 and 3.13.
const CMP_OPS: [&str; 6] = ["<", "<=", "==", "!=", ">", ">="];

/// The ten comparison operators of 3.11, where identity and membership
/// still run through `COMPARE_OP`.
const CMP_OPS_311: [&str; 10] = ["<", "<=", "==", "!=", ">", ">=", "is", "is not", "in", "not in"];

/// Arithmetic operators indexed by the `BINARY_OP` argument, in the order of
/// CPython's `_nb_ops` table (`dis` renders the symbol, not the enum name).
const NB_OPS: [&str; 26] = [
    "+", "&", "//", "<<", "@", "*", "%", "|", "**", ">>", "-", "/", "^", "+=", "&=", "//=", "<<=", "@=", "*=", "%=", "|=",
    "**=", ">>=", "-=", "/=", "^=",
];

/// The `CALL_INTRINSIC_1` operand descriptions.
const INTRINSIC_1: [&str; 6] = [
    "INTRINSIC_1_INVALID",
    "INTRINSIC_PRINT",
    "INTRINSIC_IMPORT_STAR",
    "INTRINSIC_STOPITERATION_ERROR",
    "INTRINSIC_ASYNC_GEN_WRAP",
    "INTRINSIC_UNARY_POSITIVE",
];

/// The `CALL_INTRINSIC_2` operand descriptions.
const INTRINSIC_2: [&str; 5] =
    ["INTRINSIC_2_INVALID", "INTRINSIC_PREP_RERAISE_STAR", "INTRINSIC_TYPEWITH", "INTRINSIC_TYPEVAR", "INTRINSIC_EVAL_CODE"];

/// Renders one constant in a Python-like, length-capped form.
pub(super) fn repr_value(value: &Value, codes: &[PycCodeObject]) -> String {
    match value {
        Value::None => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Int(number) => number.to_string(),
        Value::Long(text) => text.clone(),
        Value::Float(number) => number.to_string(),
        Value::Complex(text) => text.clone(),
        Value::Str(text) => quoted(text),
        Value::Bytes(bytes) => {
            let preview: String = bytes
                .iter()
                .take(48)
                .map(|byte| if byte.is_ascii_graphic() || *byte == b' ' { *byte as char } else { '.' })
                .collect();
            format!("b({} B) \"{preview}\"", bytes.len())
        }
        Value::Tuple(items) | Value::List(items) | Value::Set(items) => {
            let open = match value {
                Value::Tuple(_) => "(",
                Value::List(_) => "[",
                _ => "{",
            };
            let close = match value {
                Value::Tuple(_) => ")",
                Value::List(_) => "]",
                _ => "}",
            };
            let parts: Vec<String> = items.iter().take(8).map(|item| repr_value(item, codes)).collect();
            let tail = if items.len() > 8 { ", ..." } else { "" };
            format!("{open}{}{tail}{close}", parts.join(", "))
        }
        Value::Dict(pairs) => {
            let parts: Vec<String> = pairs
                .iter()
                .take(8)
                .map(|(key, value)| format!("{}: {}", repr_value(key, codes), repr_value(value, codes)))
                .collect();
            let tail = if pairs.len() > 8 { ", ..." } else { "" };
            format!("{{{}}}{}", parts.join(", "), tail)
        }
        Value::Code(index) => match codes.get(*index) {
            Some(code) => format!("<code {code}>", code = code.qualname),
            None => format!("<code #{index}>"),
        },
        Value::Ellipsis => "...".to_string(),
        Value::StopIter => "<stopiter>".to_string(),
        Value::Unknown(text) => format!("<{text}>"),
    }
}

/// Quotes and truncates a string constant.
fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars().take(96) {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if ch.is_control() => out.push_str(&format!("\\x{:02x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
