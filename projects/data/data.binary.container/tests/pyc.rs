//! Bytecode facts: the `marshal` graph, the `.pyc` header and the opcode table
//! selection.

use data_binary_container::pyc::{PycFormat, PycOptions, analyze, sniff};

fn marshal_word(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Writes a `marshal` small int object.
fn marshal_small_int(out: &mut Vec<u8>, value: i8) {
    out.push(b'b');
    out.push(value as u8);
}

/// Writes a `marshal` none object.
fn marshal_none(out: &mut Vec<u8>) {
    out.push(b'N');
}

/// Writes an interned short ASCII string; the parser records it as a ref.
fn marshal_string(out: &mut Vec<u8>, text: &str) {
    out.push(b'Z');
    out.push(text.len() as u8);
    out.extend_from_slice(text.as_bytes());
}

/// Writes a `marshal` string reference to an earlier interned object.
fn marshal_ref(out: &mut Vec<u8>, index: i32) {
    out.push(b'r');
    out.extend_from_slice(&index.to_le_bytes());
}

/// Writes a `marshal` bytes object.
fn marshal_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b's');
    out.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Writes a `marshal` tuple of exactly `count` items from encoded `bytes`.
fn marshal_tuple(out: &mut Vec<u8>, count: usize, bytes: &[u8]) {
    out.push(b'(');
    out.extend_from_slice(&(count as i32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Writes the empty 3.11+ `localsplus` pair for the locals tables.
fn marshal_localsplus(out: &mut Vec<u8>) {
    out.push(b')');
    out.push(0);
    marshal_bytes(out, &[]);
}

/// Field values of one synthetic 3.11 code object.
struct CodeSpec<'a> {
    stacksize: i32,
    flags: i32,
    code: &'a [u8],
    consts_count: usize,
    consts: &'a [u8],
    names_count: usize,
    names: &'a [u8],
    filename: &'a str,
    name: &'a str,
    qualname: &'a [u8],
    firstlineno: i32,
}

/// Writes one 3.11 code object with the fixed field order.
fn marshal_code(out: &mut Vec<u8>, spec: &CodeSpec<'_>) {
    out.push(0xE3);
    marshal_word(out, 0);
    marshal_word(out, 0);
    marshal_word(out, 0);
    marshal_word(out, spec.stacksize);
    marshal_word(out, spec.flags);
    marshal_bytes(out, spec.code);
    marshal_tuple(out, spec.consts_count, spec.consts);
    marshal_tuple(out, spec.names_count, spec.names);
    marshal_localsplus(out);
    marshal_string(out, spec.filename);
    marshal_string(out, spec.name);
    out.extend_from_slice(spec.qualname);
    marshal_word(out, spec.firstlineno);
    marshal_bytes(out, &[]);
    marshal_bytes(out, &[]);
}

/// Builds a synthetic module holding one nested function, as `marshal` v4.
fn pyc_fixture() -> Vec<u8> {
    let mut helper = Vec::new();
    let mut helper_consts = Vec::new();
    marshal_small_int(&mut helper_consts, 42);
    let mut helper_qualname = Vec::new();
    marshal_ref(&mut helper_qualname, 3);
    marshal_code(
        &mut helper,
        &CodeSpec {
            stacksize: 1,
            flags: 65,
            code: &[0x97, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x00, 0x53, 0x00],
            consts_count: 1,
            consts: &helper_consts,
            names_count: 0,
            names: &[],
            filename: "fixture.py",
            name: "helper",
            qualname: &helper_qualname,
            firstlineno: 2,
        },
    );
    let mut module = Vec::new();
    let mut module_consts = Vec::new();
    marshal_none(&mut module_consts);
    module_consts.extend_from_slice(&helper);
    let mut module_names = Vec::new();
    marshal_string(&mut module_names, "helper");
    let mut module_qualname = Vec::new();
    marshal_string(&mut module_qualname, "<module>");
    marshal_code(
        &mut module,
        &CodeSpec {
            stacksize: 2,
            flags: 64,
            code: &[0x97, 0x00, 0x64, 0x01, 0x84, 0x00, 0x5A, 0x00, 0x64, 0x00, 0x53, 0x00],
            consts_count: 2,
            consts: &module_consts,
            names_count: 1,
            names: &module_names,
            filename: "fixture.py",
            name: "<module>",
            qualname: &module_qualname,
            firstlineno: 1,
        },
    );
    let mut out = Vec::new();
    out.extend_from_slice(&module);
    out
}

#[test]
fn pyc_disassembles_a_synthetic_marshal_graph() {
    let report = analyze(&pyc_fixture(), &PycOptions::default()).expect("decode marshal");

    assert_eq!(report.source_format, "marshal");
    assert_eq!(report.marshal_version, None);
    assert_eq!(report.code_count, 2);
    assert_eq!(report.instruction_count, 9);
    assert!(!report.truncated);

    let helper = &report.codes[0];
    assert_eq!(helper.qualname, "helper", "the qualname resolves through the string ref");
    assert_eq!(helper.flags_text, "OPTIMIZED, NOFREE");
    assert_eq!(helper.consts[0].kind, "int");
    assert_eq!(helper.consts[0].repr, "42");
    assert_eq!(helper.instructions[0].name, "RESUME");
    assert_eq!(helper.instructions[0].caches, 4, "the zero opcode slots attach to RESUME");
    assert_eq!(helper.instructions[1].name, "LOAD_CONST");
    assert_eq!(helper.instructions[1].resolved.as_deref(), Some("42"));

    let module = &report.codes[1];
    assert_eq!(module.qualname, "<module>");
    assert_eq!(module.flags_text, "NOFREE");
    assert_eq!(module.names, vec!["helper".to_string()]);
    assert_eq!(module.consts[1].kind, "code");
    assert_eq!(module.consts[1].code_index, Some(0));
    assert_eq!(module.consts[1].repr, "<code helper>");
    assert_eq!(module.instructions[1].name, "LOAD_CONST");
    assert_eq!(module.instructions[1].resolved.as_deref(), Some("<code helper>"));
    assert_eq!(module.instructions[3].name, "STORE_NAME");
    assert_eq!(module.instructions[3].resolved.as_deref(), Some("helper"));
    assert_eq!(module.instructions[5].name, "RETURN_VALUE");
    assert_eq!(module.instructions[5].arg, None);
}

#[test]
fn pyc_reads_a_pyc_container_and_filters_by_qualname() {
    let mut bytes = vec![0xa7, 0x0d, 0x0d, 0x0a, 0, 0, 0, 0, 0x11, 0x22, 0x33, 0x44, 0x64, 0, 0, 0];
    bytes.extend(pyc_fixture());

    let report = analyze(&bytes, &PycOptions::default()).expect("decode pyc");
    assert_eq!(report.source_format, "pyc");
    assert_eq!(report.pyc_magic, Some(0x0a0d_0da7));
    assert_eq!(report.python.as_deref(), Some("CPython 3.11"));
    assert_eq!(report.code_count, 2);

    let filtered = analyze(&bytes, &PycOptions { code: Some("helper".into()), ..PycOptions::default() }).expect("filter");
    assert_eq!(filtered.codes.len(), 1);
    assert_eq!(filtered.codes[0].qualname, "helper");
    assert_eq!(filtered.code_count, 2, "the count stays the decoded total");

    let missing = analyze(&bytes, &PycOptions { code: Some("absent".into()), ..PycOptions::default() }).unwrap_err();
    assert_eq!(missing.code(), "binary/parse");

    let forced = analyze(&bytes, &PycOptions { format: PycFormat::Marshal, ..PycOptions::default() });
    assert!(forced.is_err(), "the pyc header is not a marshal stream");
}

/// A real CPython 3.12 container, compiled by 3.12.10 from a generic loop;
/// the expectations below mirror `python -m dis` instruction for instruction.
const GOLDEN_312_PYC: [u8; 421] = [
    0xcb, 0x0d, 0x0d, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc4, 0x00, 0x00, 0x00, 0xe3, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf3, 0x0a, 0x00,
    0x00, 0x00, 0x97, 0x00, 0x64, 0x00, 0x84, 0x00, 0x5a, 0x00, 0x79, 0x01, 0x29, 0x02, 0x63, 0x02, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0xf3, 0x6a, 0x00, 0x00, 0x00,
    0x97, 0x00, 0x64, 0x01, 0x7d, 0x02, 0x7c, 0x00, 0x44, 0x00, 0x5d, 0x1b, 0x00, 0x00, 0x7d, 0x03, 0x7c, 0x03, 0x80, 0x01,
    0x8c, 0x06, 0x7c, 0x03, 0x74, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x76, 0x01, 0x73, 0x01, 0x8c, 0x0f,
    0x7c, 0x02, 0x02, 0x00, 0x7c, 0x01, 0x7c, 0x03, 0x64, 0x02, 0xac, 0x03, 0xab, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x7a, 0x0d, 0x00, 0x00, 0x7d, 0x02, 0x8c, 0x1d, 0x04, 0x00, 0x7c, 0x02, 0x64, 0x01, 0x6b, 0x44, 0x00, 0x00, 0x72, 0x0b,
    0x7c, 0x02, 0x64, 0x04, 0x7a, 0x17, 0x00, 0x00, 0x7d, 0x02, 0x7c, 0x02, 0x64, 0x01, 0x6b, 0x44, 0x00, 0x00, 0x72, 0x01,
    0x8c, 0x0b, 0x7c, 0x02, 0x53, 0x00, 0x29, 0x05, 0x4e, 0xe9, 0x00, 0x00, 0x00, 0x00, 0x54, 0x29, 0x01, 0xda, 0x04, 0x66,
    0x6c, 0x61, 0x67, 0xe9, 0x01, 0x00, 0x00, 0x00, 0x29, 0x01, 0xda, 0x04, 0x73, 0x65, 0x65, 0x6e, 0x29, 0x04, 0xda, 0x05,
    0x69, 0x74, 0x65, 0x6d, 0x73, 0xda, 0x03, 0x6b, 0x65, 0x79, 0xda, 0x05, 0x74, 0x6f, 0x74, 0x61, 0x6c, 0xda, 0x01, 0x69,
    0x73, 0x04, 0x00, 0x00, 0x00, 0x20, 0x20, 0x20, 0x20, 0xfa, 0x09, 0x67, 0x6f, 0x6c, 0x64, 0x65, 0x6e, 0x2e, 0x70, 0x79,
    0xda, 0x04, 0x6c, 0x6f, 0x6f, 0x70, 0x72, 0x0c, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x73, 0x4d, 0x00, 0x00, 0x00,
    0x80, 0x00, 0xd8, 0x0c, 0x0d, 0x80, 0x45, 0xdb, 0x0d, 0x12, 0x88, 0x01, 0xd8, 0x0b, 0x0c, 0x89, 0x3d, 0x98, 0x51, 0xa4,
    0x64, 0x9a, 0x5d, 0xd8, 0x0c, 0x11, 0x91, 0x53, 0x98, 0x11, 0xa0, 0x14, 0xd4, 0x15, 0x26, 0xd1, 0x0c, 0x26, 0x89, 0x45,
    0xf0, 0x05, 0x00, 0x0e, 0x13, 0xf0, 0x06, 0x00, 0x0b, 0x10, 0x90, 0x21, 0x8a, 0x29, 0xd8, 0x08, 0x0d, 0x90, 0x11, 0x89,
    0x0a, 0x88, 0x05, 0xf0, 0x03, 0x00, 0x0b, 0x10, 0x90, 0x21, 0x8b, 0x29, 0xe0, 0x0b, 0x10, 0x80, 0x4c, 0xf3, 0x00, 0x00,
    0x00, 0x00, 0x4e, 0x29, 0x01, 0x72, 0x0c, 0x00, 0x00, 0x00, 0xa9, 0x00, 0x72, 0x0d, 0x00, 0x00, 0x00, 0x72, 0x0b, 0x00,
    0x00, 0x00, 0xda, 0x08, 0x3c, 0x6d, 0x6f, 0x64, 0x75, 0x6c, 0x65, 0x3e, 0x72, 0x0f, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x73, 0x0a, 0x00, 0x00, 0x00, 0xf0, 0x03, 0x01, 0x01, 0x01, 0xf3, 0x02, 0x07, 0x01, 0x11, 0x72, 0x0d, 0x00, 0x00,
    0x00,
];

#[test]
fn pyc_decodes_a_real_3_12_container_like_dis() {
    let report = analyze(&GOLDEN_312_PYC, &PycOptions::default()).expect("decode golden 3.12 pyc");
    assert_eq!(report.python.as_deref(), Some("CPython 3.12"));
    assert_eq!(report.opcode_table, "3.12");
    assert_eq!(report.code_count, 2);
    assert_eq!(report.instruction_count, 46);

    // Nested code objects are flattened before their parent, so `loop` is
    // index 0 and the module wrapper is index 1.
    let module = &report.codes[1];
    assert_eq!(module.qualname, "<module>");
    // RETURN_CONST is 3.12-only: the 3.11 table would name opcode 121 UNKNOWN.
    assert_eq!(module.instructions[4].name, "RETURN_CONST");
    assert_eq!(module.instructions[4].resolved.as_deref(), Some("None"));

    let function = report.codes.iter().find(|code| code.qualname == "loop").expect("loop code object");
    let at = |offset: u32| {
        function
            .instructions
            .iter()
            .find(|instruction| instruction.offset == offset)
            .unwrap_or_else(|| panic!("no instruction at {offset}"))
    };
    // FOR_ITER carries one inline cache slot; 3.12 counts it into the target.
    assert_eq!(at(10).name, "FOR_ITER");
    assert_eq!(at(10).resolved.as_deref(), Some("to 68"));
    assert_eq!(at(10).caches, 1);
    assert_eq!(at(18).name, "POP_JUMP_IF_NOT_NONE");
    assert_eq!(at(18).resolved.as_deref(), Some("to 22"));
    assert_eq!(at(20).resolved.as_deref(), Some("to 10"));
    assert_eq!(at(24).name, "LOAD_GLOBAL");
    assert_eq!(at(24).resolved.as_deref(), Some("seen"));
    assert_eq!(at(34).name, "CONTAINS_OP");
    assert_eq!(at(34).resolved.as_deref(), Some("not in"));
    assert_eq!(at(50).name, "KW_NAMES");
    assert_eq!(at(50).resolved.as_deref(), Some("(\"flag\")"));
    assert_eq!(at(52).name, "CALL");
    assert_eq!(at(60).name, "BINARY_OP");
    assert_eq!(at(60).resolved.as_deref(), Some("+="));
    // END_FOR is 3.12-only: opcode 4 was END_ASYNC_FOR's neighbour, unused in 3.11.
    assert_eq!(at(68).name, "END_FOR");
    // COMPARE_OP moved to the high nibble of the argument in 3.12.
    assert_eq!(at(74).name, "COMPARE_OP");
    assert_eq!(at(74).arg, Some(68));
    assert_eq!(at(74).resolved.as_deref(), Some(">"));
    assert_eq!(at(84).resolved.as_deref(), Some("-="));
    assert_eq!(at(104).name, "RETURN_VALUE");
}

#[test]
fn pyc_selects_the_opcode_table_by_magic() {
    // Opcode 173 is POP_JUMP_BACKWARD_IF_NOT_NONE in 3.11 and CALL_INTRINSIC_1
    // in 3.12; the same stream under the two magics must name differently.
    let mut consts = Vec::new();
    marshal_none(&mut consts);
    let mut qualname = Vec::new();
    marshal_string(&mut qualname, "<module>");
    let mut code = Vec::new();
    marshal_code(
        &mut code,
        &CodeSpec {
            stacksize: 1,
            flags: 64,
            code: &[0xad, 0x01, 0x53, 0x00],
            consts_count: 1,
            consts: &consts,
            names_count: 0,
            names: &[],
            filename: "fixture.py",
            name: "<module>",
            qualname: &qualname,
            firstlineno: 1,
        },
    );

    for (magic_low, table, name) in [(0x0da7u32, "3.11", "POP_JUMP_BACKWARD_IF_NOT_NONE"), (0x0dcb, "3.12", "CALL_INTRINSIC_1")]
    {
        let mut bytes = vec![magic_low as u8, (magic_low >> 8) as u8, 0x0d, 0x0a];
        bytes.extend([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.extend(&code);
        let report = analyze(&bytes, &PycOptions::default()).expect("decode");
        assert_eq!(report.opcode_table, table);
        assert_eq!(report.codes[0].instructions[0].name, name);
    }
}

#[test]
fn pyc_marshal_streams_use_the_pinned_or_default_table() {
    let mut consts = Vec::new();
    marshal_none(&mut consts);
    let mut qualname = Vec::new();
    marshal_string(&mut qualname, "<module>");
    let mut stream = vec![4u8];
    marshal_code(
        &mut stream,
        &CodeSpec {
            stacksize: 1,
            flags: 64,
            code: &[0xad, 0x01, 0x53, 0x00],
            consts_count: 1,
            consts: &consts,
            names_count: 0,
            names: &[],
            filename: "fixture.py",
            name: "<module>",
            qualname: &qualname,
            firstlineno: 1,
        },
    );

    let default = analyze(&stream, &PycOptions { format: PycFormat::Marshal, ..PycOptions::default() }).expect("decode");
    assert_eq!(default.opcode_table, "3.12");
    assert_eq!(default.python, None);
    assert_eq!(default.codes[0].instructions[0].name, "CALL_INTRINSIC_1");
    assert_eq!(default.codes[0].instructions[0].resolved.as_deref(), Some("INTRINSIC_PRINT"));

    let pinned =
        analyze(&stream, &PycOptions { format: PycFormat::Marshal, python: Some(311), ..PycOptions::default() }).expect("pin 3.11");
    assert_eq!(pinned.opcode_table, "3.11");
    assert_eq!(pinned.codes[0].instructions[0].name, "POP_JUMP_BACKWARD_IF_NOT_NONE");

    let bad = analyze(&stream, &PycOptions { format: PycFormat::Marshal, python: Some(310), ..PycOptions::default() });
    let error = bad.unwrap_err();
    assert_eq!(error.code(), "binary/parse");
    assert!(error.message().contains("3.10"), "the message names the version: {}", error.message());
}

#[test]
fn pyc_jump_targets_only_count_caches_from_3_12_on() {
    // FOR_ITER argument 2 followed by one cache slot: 3.11 reports the target
    // past the instruction only, 3.12 also skips the cache.
    let mut consts = Vec::new();
    marshal_none(&mut consts);
    let mut qualname = Vec::new();
    marshal_string(&mut qualname, "<module>");
    let mut stream = vec![4u8];
    marshal_code(
        &mut stream,
        &CodeSpec {
            stacksize: 1,
            flags: 64,
            code: &[0x5d, 0x02, 0x00, 0x01, 0x53, 0x00],
            consts_count: 1,
            consts: &consts,
            names_count: 0,
            names: &[],
            filename: "fixture.py",
            name: "<module>",
            qualname: &qualname,
            firstlineno: 1,
        },
    );

    let old =
        analyze(&stream, &PycOptions { format: PycFormat::Marshal, python: Some(311), ..PycOptions::default() }).expect("pin 3.11");
    assert_eq!(old.codes[0].instructions[0].resolved.as_deref(), Some("to 6"));

    let new = analyze(&stream, &PycOptions { format: PycFormat::Marshal, ..PycOptions::default() }).expect("default 3.12");
    assert_eq!(new.codes[0].instructions[0].resolved.as_deref(), Some("to 8"));
}

#[test]
fn pyc_rejects_a_container_with_an_unknown_magic() {
    let mut bytes = vec![0x12, 0x34, 0x0d, 0x0a];
    bytes.extend([0; 12]);
    bytes.extend(pyc_fixture());
    let error = analyze(&bytes, &PycOptions::default()).unwrap_err();
    assert_eq!(error.code(), "binary/parse");
    assert!(error.message().contains("unknown pyc magic"), "got {}", error.message());
    assert_eq!(error.span(), Some(data_binary_container::Span::new(0, 4)), "the magic span is reported");
}

#[test]
fn sniff_accepts_every_assigned_pyc_magic() {
    // Every assigned CPython magic is odd, so the signature must not depend on parity.
    for magic in [3413u16, 3425, 3439, 3495, 3531, 3571] {
        let header = [magic as u8, (magic >> 8) as u8, 0x0d, 0x0a, 0, 0, 0, 0];
        assert!(sniff(&header), "magic {magic} must sniff as bytecode");
    }
    assert!(sniff(&[0xE3, 0, 0, 0, 0]), "a raw code object sniffs as bytecode");
    assert!(sniff(&[4, 0, 0, 0, 0]), "a marshal version prefix sniffs as bytecode");
    assert!(!sniff(b"MZ\x90\x00\x03\x00\x00\x00"), "a PE header is not bytecode");
    assert!(!sniff(b"\x7fELF\x02\x01\x01\x00"), "an ELF header is not bytecode");
    assert!(!sniff(&[0x01, 0x01, 0x0d, 0x0a, 0, 0, 0, 0]), "a magic outside the assigned range is not bytecode");
}

#[test]
fn pyc_rejects_a_non_marshal_input() {
    let mut bytes = b"MZ\x90\x00\x03\x00\x00\x00".to_vec();
    bytes.extend([0u8; 32]);
    let error = analyze(&bytes, &PycOptions::default()).unwrap_err();
    assert_eq!(error.code(), "binary/parse");
}

#[test]
fn pyc_options_use_camel_case_defaults() {
    let defaults: PycOptions = serde_json::from_str("{}").expect("empty wire options");
    assert_eq!(defaults.format, PycFormat::Auto);
    assert_eq!(defaults.code_limit, 1024);
    assert_eq!(defaults.instruction_limit, 65_536);

    let report = analyze(&pyc_fixture(), &defaults).expect("wire decode");
    let value = serde_json::to_value(&report).expect("serialize report");
    assert_eq!(value["sourceFormat"], "marshal");
    assert_eq!(value["codeCount"], 2);
    assert_eq!(value["codes"][0]["qualname"], "helper");
    assert_eq!(value["codes"][0]["instructions"][0]["caches"], 4);
    assert_eq!(value["codes"][0]["consts"][0]["repr"], "42");
}
