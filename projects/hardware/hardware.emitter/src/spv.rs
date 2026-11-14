//! Minimal SPIR-V 1.3 binary encoder owned by this crate.
//!
//! The emitter never parses shader source and never depends on an external
//! SPIR-V toolchain: every module is assembled word by word from the built-in
//! kernels in this crate.

use std::collections::BTreeMap;

/// SPIR-V opcodes used by the built-in kernels.
pub(crate) mod op {
    pub const EXT_INST_IMPORT: u16 = 11;
    pub const EXT_INST: u16 = 12;
    pub const MEMORY_MODEL: u16 = 14;
    pub const ENTRY_POINT: u16 = 15;
    pub const EXECUTION_MODE: u16 = 16;
    pub const CAPABILITY: u16 = 17;
    pub const TYPE_VOID: u16 = 19;
    pub const TYPE_BOOL: u16 = 20;
    pub const TYPE_INT: u16 = 21;
    pub const TYPE_FLOAT: u16 = 22;
    pub const TYPE_VECTOR: u16 = 23;
    pub const TYPE_RUNTIME_ARRAY: u16 = 29;
    pub const TYPE_STRUCT: u16 = 30;
    pub const TYPE_POINTER: u16 = 32;
    pub const TYPE_FUNCTION: u16 = 33;
    pub const CONSTANT_TRUE: u16 = 41;
    pub const CONSTANT_FALSE: u16 = 42;
    pub const CONSTANT: u16 = 43;
    pub const CONSTANT_COMPOSITE: u16 = 44;
    pub const FUNCTION: u16 = 54;
    pub const FUNCTION_PARAMETER: u16 = 55;
    pub const FUNCTION_END: u16 = 56;
    pub const VARIABLE: u16 = 59;
    pub const LOAD: u16 = 61;
    pub const STORE: u16 = 62;
    pub const ACCESS_CHAIN: u16 = 65;
    pub const DECORATE: u16 = 71;
    pub const MEMBER_DECORATE: u16 = 72;
    pub const COMPOSITE_CONSTRUCT: u16 = 80;
    pub const COMPOSITE_EXTRACT: u16 = 81;
    pub const F_NEGATE: u16 = 127;
    pub const I_ADD: u16 = 128;
    pub const F_ADD: u16 = 129;
    pub const I_SUB: u16 = 130;
    pub const F_SUB: u16 = 131;
    pub const I_MUL: u16 = 132;
    pub const F_MUL: u16 = 133;
    pub const U_DIV: u16 = 134;
    pub const F_DIV: u16 = 136;
    pub const U_MOD: u16 = 137;
    pub const F_REM: u16 = 140;
    pub const F_MOD: u16 = 141;
    pub const SELECT: u16 = 169;
    pub const I_EQUAL: u16 = 170;
    pub const I_NOT_EQUAL: u16 = 171;
    pub const U_GREATER_THAN: u16 = 172;
    pub const U_GREATER_THAN_EQUAL: u16 = 173;
    pub const U_LESS_THAN: u16 = 176;
    pub const U_LESS_THAN_EQUAL: u16 = 177;
    pub const F_ORD_EQUAL: u16 = 180;
    pub const F_ORD_NOT_EQUAL: u16 = 182;
    pub const F_ORD_LESS_THAN: u16 = 184;
    pub const F_ORD_GREATER_THAN: u16 = 186;
    pub const F_ORD_LESS_THAN_EQUAL: u16 = 188;
    pub const F_ORD_GREATER_THAN_EQUAL: u16 = 190;
    pub const LOOP_MERGE: u16 = 246;
    pub const SELECTION_MERGE: u16 = 247;
    pub const LABEL: u16 = 248;
    pub const BRANCH: u16 = 249;
    pub const BRANCH_CONDITIONAL: u16 = 250;
    pub const RETURN: u16 = 253;
    pub const RETURN_VALUE: u16 = 254;
    pub const LOGICAL_OR: u16 = 166;
    pub const LOGICAL_NOT: u16 = 168;
}

/// Decoration numbers.
pub(crate) mod decoration {
    pub const BLOCK: u32 = 2;
    pub const ARRAY_STRIDE: u32 = 6;
    pub const BUILT_IN: u32 = 11;
    pub const LOCATION: u32 = 30;
    pub const BINDING: u32 = 33;
    pub const DESCRIPTOR_SET: u32 = 34;
    pub const OFFSET: u32 = 35;
    pub const NO_CONTRACTION: u32 = 42;
}

/// Built-in variable ids.
pub(crate) mod builtin {
    pub const POSITION: u32 = 0;
    pub const GLOBAL_INVOCATION_ID: u32 = 28;
    pub const VERTEX_INDEX: u32 = 42;
}

/// Storage classes.
pub(crate) mod storage {
    pub const INPUT: u32 = 1;
    pub const OUTPUT: u32 = 3;
    pub const FUNCTION: u32 = 7;
    pub const PUSH_CONSTANT: u32 = 9;
    pub const STORAGE_BUFFER: u32 = 12;
}

/// Capabilities.
pub(crate) mod capability {
    pub const SHADER: u32 = 1;
    pub const FLOAT64: u32 = 10;
}

/// Execution models.
pub(crate) mod execution_model {
    pub const VERTEX: u32 = 0;
    pub const FRAGMENT: u32 = 4;
    pub const GL_COMPUTE: u32 = 5;
}

/// Execution modes.
pub(crate) mod execution_mode {
    pub const ORIGIN_UPPER_LEFT: u32 = 7;
    pub const LOCAL_SIZE: u32 = 17;
}

/// `GLSL.std.450` extended instruction numbers.
pub(crate) mod glsl {
    pub const F_ABS: u32 = 4;
    pub const SIN: u32 = 13;
    pub const COS: u32 = 14;
    pub const ATAN: u32 = 18;
    pub const ATAN2: u32 = 25;
    pub const POW: u32 = 26;
    pub const LOG: u32 = 28;
    pub const SQRT: u32 = 31;
    pub const F_MIN: u32 = 37;
    pub const F_MAX: u32 = 40;
}

/// Append one instruction to a raw word stream.
pub(crate) fn inst(out: &mut Vec<u32>, opcode: u16, operands: &[u32]) {
    out.push(((operands.len() as u32 + 1) << 16) | u32::from(opcode));
    out.extend_from_slice(operands);
}

/// Split an `f32` into its SPIR-V literal word.
pub(crate) fn f32_word(value: f32) -> u32 {
    value.to_bits()
}

/// Split an `f64` into the two SPIR-V literal words.
pub(crate) fn f64_words(value: f64) -> [u32; 2] {
    let bits = value.to_bits();
    [bits as u32, (bits >> 32) as u32]
}

/// Module assembler for the built-in compute and raster kernels.
pub(crate) struct Spv {
    next_id: u32,
    capabilities: Vec<u32>,
    imports: Vec<u32>,
    memory_model: Vec<u32>,
    entry_points: Vec<u32>,
    execution_modes: Vec<u32>,
    annotations: Vec<u32>,
    types: Vec<u32>,
    globals: Vec<u32>,
    functions: Vec<u32>,
    vectors: BTreeMap<(u32, u32), u32>,
    pointers: BTreeMap<(u32, u32), u32>,
    constants: BTreeMap<(u32, u64), u32>,
    std450: Option<u32>,
    bytes: Option<u32>,
    /// `void` type id.
    pub void: u32,
    /// `bool` type id.
    pub bool_: u32,
    /// 32-bit unsigned integer type id.
    pub u32_: u32,
    /// 32-bit float type id.
    pub f32_: u32,
    /// 32-bit signed integer type id (created on demand).
    pub i32_: Option<u32>,
    /// 64-bit float type id (created on demand).
    pub f64_: Option<u32>,
    /// `vec2<f32>` type id.
    pub vec2_f32: u32,
    /// `vec3<u32>` type id.
    pub vec3_u32: u32,
    /// `vec3<f32>` type id.
    pub vec3_f32: u32,
    /// `vec4<f32>` type id.
    pub vec4_f32: u32,
}

impl Spv {
    /// Start an empty module with the scalar and vector types every kernel uses.
    pub fn new() -> Self {
        let mut spv = Self {
            next_id: 0,
            capabilities: Vec::new(),
            imports: Vec::new(),
            memory_model: Vec::new(),
            entry_points: Vec::new(),
            execution_modes: Vec::new(),
            annotations: Vec::new(),
            types: Vec::new(),
            globals: Vec::new(),
            functions: Vec::new(),
            vectors: BTreeMap::new(),
            pointers: BTreeMap::new(),
            constants: BTreeMap::new(),
            std450: None,
            bytes: None,
            void: 0,
            bool_: 0,
            u32_: 0,
            f32_: 0,
            i32_: None,
            f64_: None,
            vec2_f32: 0,
            vec3_u32: 0,
            vec3_f32: 0,
            vec4_f32: 0,
        };
        spv.capability(capability::SHADER);
        inst(&mut spv.memory_model, op::MEMORY_MODEL, &[0, 1]);
        spv.void = spv.fresh();
        inst(&mut spv.types, op::TYPE_VOID, &[spv.void]);
        spv.bool_ = spv.fresh();
        inst(&mut spv.types, op::TYPE_BOOL, &[spv.bool_]);
        spv.u32_ = spv.fresh();
        inst(&mut spv.types, op::TYPE_INT, &[spv.u32_, 32, 0]);
        spv.f32_ = spv.fresh();
        inst(&mut spv.types, op::TYPE_FLOAT, &[spv.f32_, 32]);
        spv.vec2_f32 = spv.vector(spv.f32_, 2);
        spv.vec3_u32 = spv.vector(spv.u32_, 3);
        spv.vec3_f32 = spv.vector(spv.f32_, 3);
        spv.vec4_f32 = spv.vector(spv.f32_, 4);
        spv
    }

    /// Allocate a fresh result id.
    pub fn fresh(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// Declare a capability (duplicates are collapsed).
    pub fn capability(&mut self, capability: u32) {
        if !self.capabilities.iter().any(|entry| *entry == capability) {
            inst(&mut self.capabilities, op::CAPABILITY, &[capability]);
        }
    }

    /// 32-bit signed integer type id.
    pub fn i32(&mut self) -> u32 {
        if let Some(id) = self.i32_ {
            return id;
        }
        let id = self.fresh();
        inst(&mut self.types, op::TYPE_INT, &[id, 32, 1]);
        self.i32_ = Some(id);
        id
    }

    /// 64-bit float type id, adding the `Float64` capability on first use.
    pub fn f64(&mut self) -> u32 {
        if let Some(id) = self.f64_ {
            return id;
        }
        self.capability(capability::FLOAT64);
        let id = self.fresh();
        inst(&mut self.types, op::TYPE_FLOAT, &[id, 64]);
        self.f64_ = Some(id);
        id
    }

    /// Vector type over `component` with `count` lanes.
    pub fn vector(&mut self, component: u32, count: u32) -> u32 {
        if let Some(id) = self.vectors.get(&(component, count)) {
            return *id;
        }
        let id = self.fresh();
        inst(&mut self.types, op::TYPE_VECTOR, &[id, component, count]);
        self.vectors.insert((component, count), id);
        id
    }

    /// Pointer type for `storage` class and `pointee` type.
    pub fn pointer(&mut self, storage_class: u32, pointee: u32) -> u32 {
        if let Some(id) = self.pointers.get(&(storage_class, pointee)) {
            return *id;
        }
        let id = self.fresh();
        inst(&mut self.types, op::TYPE_POINTER, &[id, storage_class, pointee]);
        self.pointers.insert((storage_class, pointee), id);
        id
    }

    /// Runtime array of `element` with the given `stride` in bytes.
    pub fn runtime_array(&mut self, element: u32, stride: u32) -> u32 {
        let id = self.fresh();
        inst(&mut self.types, op::TYPE_RUNTIME_ARRAY, &[id, element]);
        inst(&mut self.annotations, op::DECORATE, &[id, decoration::ARRAY_STRIDE, stride]);
        id
    }

    /// Block-decorated struct with explicit member offsets.
    pub fn block_struct(&mut self, members: &[(u32, u32)]) -> u32 {
        let id = self.fresh();
        let mut operands = Vec::with_capacity(members.len() + 1);
        operands.push(id);
        operands.extend(members.iter().map(|(ty, _)| *ty));
        inst(&mut self.types, op::TYPE_STRUCT, &operands);
        inst(&mut self.annotations, op::DECORATE, &[id, decoration::BLOCK]);
        for (index, (_, offset)) in members.iter().enumerate() {
            inst(
                &mut self.annotations,
                op::MEMBER_DECORATE,
                &[id, index as u32, decoration::OFFSET, *offset],
            );
        }
        id
    }

    /// Function type over `parameters` returning `result`.
    pub fn function_type(&mut self, result: u32, parameters: &[u32]) -> u32 {
        let id = self.fresh();
        let mut operands = Vec::with_capacity(parameters.len() + 2);
        operands.push(id);
        operands.push(result);
        operands.extend_from_slice(parameters);
        inst(&mut self.types, op::TYPE_FUNCTION, &operands);
        id
    }

    /// Declare an `Output`/`Input`/`StorageBuffer`/`PushConstant` variable.
    pub fn global(&mut self, pointer_type: u32, storage_class: u32) -> u32 {
        let id = self.fresh();
        inst(&mut self.globals, op::VARIABLE, &[pointer_type, id, storage_class]);
        id
    }

    /// Decorate `target` with a single-literal decoration.
    pub fn decorate(&mut self, target: u32, decoration_value: u32, literals: &[u32]) {
        let mut operands = Vec::with_capacity(literals.len() + 2);
        operands.push(target);
        operands.push(decoration_value);
        operands.extend_from_slice(literals);
        inst(&mut self.annotations, op::DECORATE, &operands);
    }

    /// Declare an entry point with its interface variables.
    pub fn entry_point(
        &mut self,
        model: u32,
        function: u32,
        name: &str,
        interface: &[u32],
    ) {
        let mut operands = Vec::with_capacity(interface.len() + 4);
        operands.push(model);
        operands.push(function);
        for word in name_words(name) {
            operands.push(word);
        }
        operands.extend_from_slice(interface);
        inst(&mut self.entry_points, op::ENTRY_POINT, &operands);
    }

    /// Declare an execution mode for `function`.
    pub fn execution_mode(&mut self, function: u32, mode: u32, literals: &[u32]) {
        let mut operands = Vec::with_capacity(literals.len() + 2);
        operands.push(function);
        operands.push(mode);
        operands.extend_from_slice(literals);
        inst(&mut self.execution_modes, op::EXECUTION_MODE, &operands);
    }

    /// `GLSL.std.450` import id, declared on first use.
    pub fn std450(&mut self) -> u32 {
        if let Some(id) = self.std450 {
            return id;
        }
        let id = self.fresh();
        let mut operands = vec![id];
        operands.extend_from_slice(&name_words("GLSL.std.450"));
        inst(&mut self.imports, op::EXT_INST_IMPORT, &operands);
        self.std450 = Some(id);
        id
    }

    /// 32-bit unsigned integer constant.
    pub fn const_u32(&mut self, value: u32) -> u32 {
        self.constant(self.u32_, u64::from(value), &[value])
    }

    /// 32-bit signed integer constant.
    pub fn const_i32(&mut self, value: i32) -> u32 {
        let ty = self.i32();
        self.constant(ty, u64::from(value as u32), &[value as u32])
    }

    /// 32-bit float constant.
    pub fn const_f32(&mut self, value: f32) -> u32 {
        self.constant(self.f32_, u64::from(value.to_bits()), &[f32_word(value)])
    }

    /// 64-bit float constant.
    pub fn const_f64(&mut self, value: f64) -> u32 {
        let ty = self.f64();
        self.constant(ty, value.to_bits(), &f64_words(value))
    }

    /// Boolean constant.
    pub fn const_bool(&mut self, value: bool) -> u32 {
        let ty = self.bool_;
        let opcode = if value { op::CONSTANT_TRUE } else { op::CONSTANT_FALSE };
        if let Some(id) = self.constants.get(&(ty, u64::from(value))) {
            return *id;
        }
        let id = self.fresh();
        inst(&mut self.globals, opcode, &[ty, id]);
        self.constants.insert((ty, u64::from(value)), id);
        id
    }

    /// Composite constant.
    pub fn const_composite(&mut self, ty: u32, parts: &[u32]) -> u32 {
        let id = self.fresh();
        let mut operands = Vec::with_capacity(parts.len() + 2);
        operands.push(ty);
        operands.push(id);
        operands.extend_from_slice(parts);
        inst(&mut self.globals, op::CONSTANT_COMPOSITE, &operands);
        id
    }

    fn constant(&mut self, ty: u32, key: u64, literal: &[u32]) -> u32 {
        if let Some(id) = self.constants.get(&(ty, key)) {
            return *id;
        }
        let id = self.fresh();
        let mut operands = Vec::with_capacity(literal.len() + 2);
        operands.push(ty);
        operands.push(id);
        operands.extend_from_slice(literal);
        inst(&mut self.globals, op::CONSTANT, &operands);
        self.constants.insert((ty, key), id);
        id
    }

    /// Append a finished function body (header, labels, instructions and
    /// `OpFunctionEnd`) to the module.
    pub fn push_function(&mut self, mut words: Vec<u32>) {
        self.functions.append(&mut words);
    }

    /// Assemble the module into SPIR-V words.
    fn words(mut self) -> Vec<u32> {
        let bound = self.next_id + 1;
        let mut out = vec![0x0723_0203, 0x0001_0300, 0, bound, 0];
        out.append(&mut self.capabilities);
        out.append(&mut self.imports);
        out.append(&mut self.memory_model);
        out.append(&mut self.entry_points);
        out.append(&mut self.execution_modes);
        out.append(&mut self.annotations);
        out.append(&mut self.types);
        out.append(&mut self.globals);
        out.append(&mut self.functions);
        out
    }

    /// Assemble the module into little-endian SPIR-V bytes.
    pub fn finish(self) -> Vec<u8> {
        self.words().into_iter().flat_map(u32::to_le_bytes).collect()
    }
}

impl Default for Spv {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a SPIR-V literal string into NUL-terminated words.
fn name_words(name: &str) -> Vec<u32> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Append `OpCompositeExtract` and return the result id.
pub(crate) fn extract(
    spv: &mut Spv,
    code: &mut Vec<u32>,
    result: u32,
    base: u32,
    index: u32,
) -> u32 {
    let id = spv.fresh();
    inst(code, op::COMPOSITE_EXTRACT, &[result, id, base, index]);
    id
}

/// Append `OpCompositeConstruct` and return the result id.
pub(crate) fn construct(spv: &mut Spv, code: &mut Vec<u32>, result: u32, parts: &[u32]) -> u32 {
    let id = spv.fresh();
    let mut operands = Vec::with_capacity(parts.len() + 2);
    operands.push(result);
    operands.push(id);
    operands.extend_from_slice(parts);
    inst(code, op::COMPOSITE_CONSTRUCT, &operands);
    id
}

/// Append `OpSelect` and return the result id.
pub(crate) fn select(
    spv: &mut Spv,
    code: &mut Vec<u32>,
    result: u32,
    condition: u32,
    accept: u32,
    reject: u32,
) -> u32 {
    let id = spv.fresh();
    inst(code, op::SELECT, &[result, id, condition, accept, reject]);
    id
}

/// Append `OpAccessChain` and return the result id.
pub(crate) fn chain(
    spv: &mut Spv,
    code: &mut Vec<u32>,
    result: u32,
    base: u32,
    indices: &[u32],
) -> u32 {
    let id = spv.fresh();
    let mut operands = Vec::with_capacity(indices.len() + 3);
    operands.push(result);
    operands.push(id);
    operands.push(base);
    operands.extend_from_slice(indices);
    inst(code, op::ACCESS_CHAIN, &operands);
    id
}

/// Append `OpLoad` and return the result id.
pub(crate) fn load(spv: &mut Spv, code: &mut Vec<u32>, result: u32, pointer: u32) -> u32 {
    let id = spv.fresh();
    inst(code, op::LOAD, &[result, id, pointer]);
    id
}

/// Append `OpStore`.
pub(crate) fn store(code: &mut Vec<u32>, pointer: u32, value: u32) {
    inst(code, op::STORE, &[pointer, value]);
}

/// Append an arithmetic or comparison instruction and return the result id.
pub(crate) fn binary(
    spv: &mut Spv,
    code: &mut Vec<u32>,
    result: u32,
    opcode: u16,
    lhs: u32,
    rhs: u32,
) -> u32 {
    let id = spv.fresh();
    inst(code, opcode, &[result, id, lhs, rhs]);
    id
}

/// Append a unary instruction and return the result id.
pub(crate) fn unary(
    spv: &mut Spv,
    code: &mut Vec<u32>,
    result: u32,
    opcode: u16,
    value: u32,
) -> u32 {
    let id = spv.fresh();
    inst(code, opcode, &[result, id, value]);
    id
}

/// Append a `GLSL.std.450` extended instruction and return the result id.
pub(crate) fn ext(
    spv: &mut Spv,
    code: &mut Vec<u32>,
    result: u32,
    instruction: u32,
    args: &[u32],
) -> u32 {
    let set = spv.std450();
    let id = spv.fresh();
    let mut operands = Vec::with_capacity(args.len() + 4);
    operands.push(result);
    operands.push(id);
    operands.push(set);
    operands.push(instruction);
    operands.extend_from_slice(args);
    inst(code, op::EXT_INST, &operands);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_uses_spirv_magic_and_bound() {
        let mut spv = Spv::new();
        let ty = spv.function_type(spv.void, &[]);
        let _ = ty;
        let bytes = spv.finish();
        assert_eq!(&bytes[0..4], &[0x03, 0x02, 0x23, 0x07]);
        let bound = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        assert!(bound > 1);
    }
}
