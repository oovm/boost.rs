//! DXBC container and SM5 token encoding.
//!
//! Shader modules are assembled from container chunks (`RDEF`, `ISGN`, `OSGN`,
//! `SHEX`) whose payloads are produced by the typed token helpers below.
//! Nothing in this module writes HLSL text or calls a shader compiler.

/// Container magic.
const MAGIC: [u8; 4] = *b"DXBC";
/// Container header version.
const CONTAINER_VERSION: u32 = 1;
/// Bytes before the chunk offset table: magic, digest, version, size, count.
const HEADER_LEN: usize = 4 + 16 + 4 + 4 + 4;

/// One DXBC chunk.
pub(crate) struct Chunk {
    pub(crate) fourcc: [u8; 4],
    pub(crate) data: Vec<u8>,
}

impl Chunk {
    pub(crate) fn new(fourcc: [u8; 4], data: Vec<u8>) -> Self {
        Self { fourcc, data }
    }
}

/// Assembles chunks into a DXBC container.
///
/// Chunk payloads are padded to a four-byte boundary, which keeps every chunk
/// offset in the table dword aligned. The digest over the assembled bytes is
/// filled in last: Direct3D refuses a module whose digest does not match its
/// contents.
pub(crate) fn container(chunks: &[Chunk]) -> Vec<u8> {
    let sizes: Vec<usize> = chunks.iter().map(|chunk| chunk.data.len().div_ceil(4) * 4).collect();
    let table_end = HEADER_LEN + 4 * chunks.len();
    let total = chunks.iter().zip(&sizes).fold(table_end, |sum, (_, size)| sum + 8 + size);

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&CONTAINER_VERSION.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());

    let mut offset = table_end;
    for size in &sizes {
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += 8 + size;
    }
    for (chunk, size) in chunks.iter().zip(&sizes) {
        out.extend_from_slice(&chunk.fourcc);
        out.extend_from_slice(&(*size as u32).to_le_bytes());
        out.extend_from_slice(&chunk.data);
        out.resize(out.len() + (size - chunk.data.len()), 0);
    }
    debug_assert_eq!(out.len(), total);

    let digest = digest(&out);
    out[4..20].copy_from_slice(&digest);
    out
}

const MD5_INIT: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, //
];
#[rustfmt::skip]
const MD5_SINE: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, 0xf57c_0faf, 0x4787_c62a, 0xa830_4613, 0xfd46_9501,
    0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, 0x6b90_1122, 0xfd98_7193, 0xa679_438e, 0x49b4_0821,
    0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, 0xd62f_105d, 0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8,
    0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed, 0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a,
    0xfffa_3942, 0x8771_f681, 0x6d9d_6122, 0xfde5_380c, 0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70,
    0x289b_7ec6, 0xeaa1_27fa, 0xd4ef_3085, 0x0488_1d05, 0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665,
    0xf429_2244, 0x432a_ff97, 0xab94_23a7, 0xfc93_a039, 0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, 0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb, 0xeb86_d391,
];

/// Runs one MD5 compression round over a 64-byte block.
fn md5_block(state: &mut [u32; 4], block: &[u8]) {
    let mut words = [0u32; 16];
    for (index, word) in words.iter_mut().enumerate() {
        let start = index * 4;
        *word = u32::from_le_bytes([block[start], block[start + 1], block[start + 2], block[start + 3]]);
    }

    let [mut a, mut b, mut c, mut d] = *state;
    for round in 0..64 {
        let (mixed, word) = match round / 16 {
            0 => ((b & c) | (!b & d), round),
            1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
            2 => (b ^ c ^ d, (3 * round + 5) % 16),
            _ => (c ^ (b | !d), (7 * round) % 16),
        };
        let sum = a.wrapping_add(mixed).wrapping_add(MD5_SINE[round]).wrapping_add(words[word]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(sum.rotate_left(MD5_SHIFTS[round]));
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// Digest stored in a container's header: the MD5 block function over every byte
/// after the header, finished with the padding the bytecode compiler uses.
fn digest(blob: &[u8]) -> [u8; 16] {
    let data = &blob[20..];
    let bits = (data.len() as u32).wrapping_mul(8);
    let mut state = MD5_INIT;

    let mut blocks = data.chunks_exact(64);
    for block in &mut blocks {
        md5_block(&mut state, block);
    }
    let rest = blocks.remainder();

    let mut last = [0u8; 64];
    if rest.len() < 56 {
        last[4..4 + rest.len()].copy_from_slice(rest);
        last[4 + rest.len()] = 0x80;
    }
    else {
        let mut first = [0u8; 64];
        first[..rest.len()].copy_from_slice(rest);
        first[rest.len()] = 0x80;
        md5_block(&mut state, &first);
    }
    last[..4].copy_from_slice(&bits.to_le_bytes());
    last[60..].copy_from_slice(&((bits >> 2) | 1).to_le_bytes());
    md5_block(&mut state, &last);

    let mut digest = [0u8; 16];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    digest
}

/// Shader stage of a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Vertex,
    Pixel,
    Compute,
}

impl Stage {
    /// Shader type written to the `SHEX` version token.
    const fn bytecode_type(self) -> u32 {
        match self {
            Self::Pixel => 0,
            Self::Vertex => 1,
            Self::Compute => 5,
        }
    }

    /// Stage tag written to the `RDEF` version token.
    const fn reflection_type(self) -> u32 {
        match self {
            Self::Pixel => 0xffff,
            Self::Vertex => 0xfffe,
            Self::Compute => 0x4353,
        }
    }

    /// Stage tag stored at offset `0x1c` of the `RDEF` header.
    const fn reflection_tag(self) -> u32 {
        match self {
            Self::Pixel | Self::Vertex => u32::from_le_bytes(*b"RD11"),
            Self::Compute => 0x2544_1313,
        }
    }

    /// Stage flags stored at offset `0x14` of the `RDEF` header.
    const fn reflection_flags(self) -> u32 {
        match self {
            Self::Pixel | Self::Vertex => 0x8100,
            Self::Compute => 0x8500,
        }
    }
}

/// SM5 instruction opcode (`D3D10_SB_OPCODE_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Opcode {
    /// `breakc` with the non-zero test control bit.
    BreakCNz = 3,
    /// `endif`.
    EndIf = 21,
    /// `endloop`.
    EndLoop = 22,
    /// `iadd`.
    IAdd = 30,
    /// `if_nz`.
    IfNz = 31,
    /// `imad`.
    IMad = 35,
    /// `imul`, which writes a high and a low half.
    IMul = 38,
    /// `ishl`.
    IShl = 41,
    /// `loop`.
    Loop = 48,
    /// `mad`.
    Mad = 50,
    /// `mov`.
    Mov = 54,
    /// `ret`.
    Ret = 62,
    /// `udiv`, which writes a quotient and a remainder.
    UDiv = 78,
    /// `uge`.
    Uge = 80,
    /// `dcl_constantbuffer`.
    DclConstantBuffer = 89,
    /// `dcl_input`.
    DclInput = 95,
    /// `dcl_input_sgv`.
    DclInputSgv = 96,
    /// `dcl_input_ps`.
    DclInputPs = 98,
    /// `dcl_output`.
    DclOutput = 101,
    /// `dcl_output_sgv`.
    DclOutputSgv = 103,
    /// `dcl_temps`.
    DclTemps = 104,
    /// `dcl_indexableTemp`.
    DclIndexableTemp = 105,
    /// `dcl_globalFlags`.
    DclGlobalFlags = 106,
    /// `dcl_thread_group`.
    DclThreadGroup = 155,
    /// `dcl_uav_raw`.
    DclUavRaw = 157,
    /// `dcl_resource_raw`.
    DclResourceRaw = 161,
    /// `ld_raw`.
    LdRaw = 165,
    /// `store_raw`.
    StoreRaw = 166,
}

/// Instruction control bit: `refactoringAllowed` in `dcl_globalFlags`.
pub(crate) const REFACTORING_ALLOWED: u32 = 1;
/// Instruction control bit: conditional instructions test for non-zero.
pub(crate) const TEST_NONZERO: u32 = 0x80;
/// Instruction control bit: linear interpolation in `dcl_input_ps`.
pub(crate) const LINEAR_INTERPOLATION: u32 = 2;

/// Register file of an operand (`D3D10_SB_OPERAND_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Register {
    /// General purpose temporary (`r#`).
    Temp,
    /// Shader input (`v#`).
    Input,
    /// Shader output (`o#`).
    Output,
    /// Indexable temporary array (`x#`).
    IndexableTemp,
    /// Inline 32-bit immediate (`l(...)`).
    Immediate32,
    /// Raw shader resource view (`t#`).
    Resource,
    /// Constant buffer (`cb#`).
    ConstantBuffer,
    /// Raw unordered access view (`u#`).
    Uav,
    /// Discarded destination (`null`).
    Null,
    /// Compute thread id system value (`vThreadID`).
    ThreadId,
}

impl Register {
    const fn code(self) -> u32 {
        match self {
            Self::Temp => 0,
            Self::Input => 1,
            Self::Output => 2,
            Self::IndexableTemp => 3,
            Self::Immediate32 => 4,
            Self::Resource => 7,
            Self::ConstantBuffer => 8,
            Self::Null => 13,
            Self::Uav => 30,
            Self::ThreadId => 32,
        }
    }
}

/// Component selection of an operand token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Select {
    /// Single component without selection bits.
    Single,
    /// Four-component write mask.
    Mask(u8),
    /// Four-component swizzle.
    Swizzle([u8; 4]),
    /// One replicated component.
    Scalar(u8),
}

impl Select {
    /// Swizzle `.xyxx`, used by declarations and indexable temporary reads.
    pub(crate) const XYXX: Select = Select::Swizzle([0, 1, 0, 0]);
    /// Swizzle `.xyzw`.
    pub(crate) const ALL: Select = Select::Swizzle([0, 1, 2, 3]);
    /// Component selection is unused: four-component form with an empty mask.
    pub(crate) const UNUSED: Select = Select::Mask(0);

    const fn bits(self) -> (u32, u32) {
        match self {
            Self::Single => (1, 0),
            Self::Mask(mask) => (2, ((mask & 0xf) as u32) << 4),
            Self::Swizzle(parts) => {
                let packed = (parts[0] & 3) as u32 | ((parts[1] & 3) as u32) << 2 | ((parts[2] & 3) as u32) << 4 | ((parts[3] & 3) as u32) << 6;
                (2, (1 << 2) | (packed << 4))
            }
            Self::Scalar(component) => (2, (2 << 2) | (((component & 3) as u32) << 4)),
        }
    }
}

/// One index level of an operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Index {
    /// Literal register index.
    Imm(u32),
    /// Index read from one component of a temporary.
    Relative(u32, u8),
}

impl Index {
    const fn repr(self) -> u32 {
        match self {
            Self::Imm(_) => 0,
            Self::Relative(..) => 2,
        }
    }

    fn encode(self, words: &mut Vec<u32>) {
        match self {
            Self::Imm(value) => words.push(value),
            Self::Relative(slot, component) => Operand::temp(slot, Select::Scalar(component)).encode(words),
        }
    }
}

/// A source or destination operand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Operand {
    register: Register,
    indices: Vec<Index>,
    select: Option<Select>,
    immediates: Vec<u32>,
}

impl Operand {
    fn new(register: Register, indices: Vec<Index>, select: Select) -> Self {
        Self { register, indices, select: Some(select), immediates: Vec::new() }
    }

    /// Temporary register `r#`.
    pub(crate) fn temp(slot: u32, select: Select) -> Self {
        Self::new(Register::Temp, vec![Index::Imm(slot)], select)
    }

    /// Input register `v#`.
    pub(crate) fn input(slot: u32, select: Select) -> Self {
        Self::new(Register::Input, vec![Index::Imm(slot)], select)
    }

    /// Output register `o#`.
    pub(crate) fn output(slot: u32, select: Select) -> Self {
        Self::new(Register::Output, vec![Index::Imm(slot)], select)
    }

    /// Compute thread id system value; system values carry no register index.
    pub(crate) fn thread_id(select: Select) -> Self {
        Self::new(Register::ThreadId, Vec::new(), select)
    }

    /// Constant buffer read `cb#<element>`.
    pub(crate) fn constant_buffer(slot: u32, element: u32, select: Select) -> Self {
        Self::new(Register::ConstantBuffer, vec![Index::Imm(slot), Index::Imm(0), Index::Imm(element)], select)
    }

    /// Resource binding operand of a `dcl_*` instruction.
    pub(crate) fn binding(register: Register, slot: u32, select: Select) -> Self {
        Self::new(register, vec![Index::Imm(slot), Index::Imm(slot), Index::Imm(slot)], select)
    }

    /// Raw buffer operand of `ld_raw` / `store_raw`: both index levels repeat the slot.
    pub(crate) fn bound(register: Register, slot: u32, select: Select) -> Self {
        Self::new(register, vec![Index::Imm(slot), Index::Imm(slot)], select)
    }

    /// Indexable temporary access `x0[<index>]`.
    pub(crate) fn indexable_temp(select: Select, index: Index) -> Self {
        Self::new(Register::IndexableTemp, vec![Index::Imm(0), index], select)
    }

    /// Inline immediate constant. Four values select the four-component form.
    pub(crate) fn immediate(values: &[u32]) -> Self {
        let select = if values.len() == 4 { Select::Mask(0) } else { Select::Single };
        Self { register: Register::Immediate32, indices: Vec::new(), select: Some(select), immediates: values.to_vec() }
    }

    /// Null destination, used for the discarded half of a wide operation.
    pub(crate) fn null() -> Self {
        Self { register: Register::Null, indices: Vec::new(), select: None, immediates: Vec::new() }
    }

    fn encode(&self, words: &mut Vec<u32>) {
        let (count, select) = self.select.map_or((0, 0), Select::bits);
        let mut token = count | select;
        token |= (self.register.code() & 0xff) << 12;
        token |= (self.indices.len() as u32 & 0x3) << 20;
        for (level, index) in self.indices.iter().enumerate() {
            token |= index.repr() << (22 + 3 * level);
        }
        words.push(token);

        if self.register == Register::Immediate32 {
            words.extend_from_slice(&self.immediates);
            return;
        }
        for index in &self.indices {
            index.encode(words);
        }
    }
}

/// Appends SM5 instructions and finishes as a `SHEX` chunk payload.
pub(crate) struct ShaderWriter {
    stage: Stage,
    minor: u32,
    words: Vec<u32>,
}

impl ShaderWriter {
    pub(crate) fn new(stage: Stage, minor: u32) -> Self {
        Self { stage, minor, words: Vec::new() }
    }

    fn instruction(&mut self, opcode: Opcode, control: u32, build: impl FnOnce(&mut Vec<u32>)) {
        let start = self.words.len();
        self.words.push(0);
        build(&mut self.words);
        let length = (self.words.len() - start) as u32;
        self.words[start] = opcode as u32 | (control << 11) | (length << 24);
    }

    /// Appends an instruction followed by its operands.
    pub(crate) fn op(&mut self, opcode: Opcode, control: u32, operands: &[Operand]) {
        self.instruction(opcode, control, |words| {
            for operand in operands {
                operand.encode(words);
            }
        });
    }

    /// `dcl_globalFlags refactoringAllowed`.
    pub(crate) fn refactoring_allowed(&mut self) {
        self.instruction(Opcode::DclGlobalFlags, REFACTORING_ALLOWED, |_| {});
    }

    /// `dcl_constantbuffer cb<slot>[<vec4s>], immediateIndexed`.
    pub(crate) fn dcl_constant_buffer(&mut self, slot: u32, vec4s: u32) {
        self.instruction(Opcode::DclConstantBuffer, 0, |words| {
            Operand::binding(Register::ConstantBuffer, slot, Select::ALL).encode(words);
            words.push(vec4s);
            words.push(0);
        });
    }

    /// `dcl_resource_raw t<slot>`.
    pub(crate) fn dcl_resource_raw(&mut self, slot: u32) {
        self.instruction(Opcode::DclResourceRaw, 0, |words| {
            Operand::binding(Register::Resource, slot, Select::ALL).encode(words);
            words.push(0);
        });
    }

    /// `dcl_uav_raw u<slot>`.
    pub(crate) fn dcl_uav_raw(&mut self, slot: u32) {
        self.instruction(Opcode::DclUavRaw, 0, |words| {
            Operand::binding(Register::Uav, slot, Select::ALL).encode(words);
            words.push(0);
        });
    }

    /// `dcl_input vThreadID.x`.
    pub(crate) fn dcl_thread_id(&mut self) {
        self.op(Opcode::DclInput, 0, &[Operand::thread_id(Select::Mask(0b0001))]);
    }

    /// `dcl_input_sgv v<slot>.<components>, <system value>`.
    pub(crate) fn dcl_input_sgv(&mut self, slot: u32, components: u8, system_value: u32) {
        self.instruction(Opcode::DclInputSgv, 0, |words| {
            Operand::input(slot, Select::Mask(components)).encode(words);
            words.push(system_value);
        });
    }

    /// `dcl_input_ps v<slot>.<components>` with linear interpolation.
    pub(crate) fn dcl_input_ps_linear(&mut self, slot: u32, components: u8) {
        self.op(Opcode::DclInputPs, LINEAR_INTERPOLATION, &[Operand::input(slot, Select::Mask(components))]);
    }

    /// `dcl_output o<slot>.<components>`.
    pub(crate) fn dcl_output(&mut self, slot: u32, components: u8) {
        self.op(Opcode::DclOutput, 0, &[Operand::output(slot, Select::Mask(components))]);
    }

    /// `dcl_output_sgv o<slot>.<components>, <system value>`.
    pub(crate) fn dcl_output_sgv(&mut self, slot: u32, components: u8, system_value: u32) {
        self.instruction(Opcode::DclOutputSgv, 0, |words| {
            Operand::output(slot, Select::Mask(components)).encode(words);
            words.push(system_value);
        });
    }

    /// `dcl_temps <count>`.
    pub(crate) fn dcl_temps(&mut self, count: u32) {
        self.instruction(Opcode::DclTemps, 0, |words| words.push(count));
    }

    /// `dcl_indexableTemp x<slot>[<count>].<components>`.
    pub(crate) fn dcl_indexable_temp(&mut self, slot: u32, count: u32, components: u32) {
        self.instruction(Opcode::DclIndexableTemp, 0, |words| {
            words.push(slot);
            words.push(count);
            words.push(components);
        });
    }

    /// `dcl_thread_group <x>, <y>, <z>`.
    pub(crate) fn dcl_thread_group(&mut self, x: u32, y: u32, z: u32) {
        self.instruction(Opcode::DclThreadGroup, 0, |words| {
            words.push(x);
            words.push(y);
            words.push(z);
        });
    }

    /// Finishes the program as a `SHEX` chunk payload.
    pub(crate) fn finish(self) -> Vec<u8> {
        let mut words = Vec::with_capacity(self.words.len() + 2);
        words.push((self.stage.bytecode_type() << 16) | (5 << 4) | self.minor);
        words.push(0);
        words.extend_from_slice(&self.words);
        words[1] = words.len() as u32;
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }
}

/// Buffer register kind of a bound resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BufferRegister {
    /// Raw `ByteAddressBuffer` bound to `t#`.
    Srv,
    /// Raw `RWByteAddressBuffer` bound to `u#`.
    Uav,
}

/// One variable inside a reflected constant buffer.
pub(crate) struct Variable {
    pub(crate) name: String,
    pub(crate) offset: u32,
    pub(crate) used: bool,
}

/// One reflected constant buffer.
pub(crate) struct ConstantBuffer {
    pub(crate) name: String,
    pub(crate) size: u32,
    pub(crate) variables: Vec<Variable>,
}

/// One reflected raw buffer binding.
pub(crate) struct BoundBuffer {
    pub(crate) name: String,
    pub(crate) register: BufferRegister,
    pub(crate) slot: u32,
}

/// Reflection metadata carried by the `RDEF` chunk.
pub(crate) struct Reflection {
    pub(crate) stage: Stage,
    pub(crate) minor: u32,
    pub(crate) creator: String,
    pub(crate) constant_buffers: Vec<ConstantBuffer>,
    pub(crate) buffers: Vec<BoundBuffer>,
}

const RDEF_HEADER_LEN: u32 = 0x3c;
const RDEF_RESOURCE_LEN: u32 = 40;
const RDEF_CONSTANT_BUFFER_LEN: u32 = 24;
const RDEF_VARIABLE_LEN: u32 = 40;
const RDEF_TYPE_LEN: u32 = 36;

/// String table placed after the fixed-layout records of a chunk.
struct Strings {
    data: Vec<u8>,
    base: u32,
}

impl Strings {
    fn add(&mut self, text: &str) -> u32 {
        let offset = self.base + self.data.len() as u32;
        self.data.extend_from_slice(text.as_bytes());
        self.data.push(0);
        offset
    }

    fn align(&mut self) {
        while self.data.len() % 4 != 0 {
            self.data.push(0);
        }
    }
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Builds the `RDEF` chunk payload.
pub(crate) fn reflection_chunk(reflection: &Reflection) -> Vec<u8> {
    let buffers = &reflection.buffers;
    let constant_buffers = &reflection.constant_buffers;
    let resource_count = (buffers.len() + constant_buffers.len()) as u32;
    let variable_count: u32 = constant_buffers.iter().map(|buffer| buffer.variables.len() as u32).sum();

    let resource_offset = RDEF_HEADER_LEN;
    let constant_buffer_offset = resource_offset + resource_count * RDEF_RESOURCE_LEN;
    let variable_offset = constant_buffer_offset + constant_buffers.len() as u32 * RDEF_CONSTANT_BUFFER_LEN;
    let mut cursor = variable_offset + variable_count * RDEF_VARIABLE_LEN;

    let mut strings = Strings { data: Vec::new(), base: cursor };
    let buffer_names: Vec<u32> = buffers.iter().map(|buffer| strings.add(&buffer.name)).collect();
    let constant_buffer_names: Vec<u32> = constant_buffers.iter().map(|buffer| strings.add(&buffer.name)).collect();
    let variable_names: Vec<Vec<u32>> =
        constant_buffers.iter().map(|buffer| buffer.variables.iter().map(|variable| strings.add(&variable.name)).collect()).collect();
    let type_name = if variable_count == 0 { 0 } else { strings.add("dword") };
    strings.align();
    cursor += strings.data.len() as u32;

    let type_offset = cursor;
    let creator_offset = if variable_count == 0 { type_offset } else { type_offset + RDEF_TYPE_LEN };
    cursor = creator_offset + reflection.creator.len() as u32 + 1;

    let mut out = Vec::with_capacity(cursor as usize);
    push_u32(&mut out, constant_buffers.len() as u32);
    push_u32(&mut out, if constant_buffers.is_empty() { 0 } else { constant_buffer_offset });
    push_u32(&mut out, resource_count);
    push_u32(&mut out, resource_offset);
    push_u32(&mut out, (reflection.stage.reflection_type() << 16) | (5 << 8) | (reflection.minor & 0xf));
    push_u32(&mut out, reflection.stage.reflection_flags());
    push_u32(&mut out, creator_offset);
    push_u32(&mut out, reflection.stage.reflection_tag());
    // Record sizes and section strides of the sections that follow the header.
    push_u32(&mut out, resource_offset);
    push_u32(&mut out, RDEF_CONSTANT_BUFFER_LEN);
    push_u32(&mut out, if variable_count == 0 { 32 } else { RDEF_VARIABLE_LEN });
    push_u32(&mut out, RDEF_RESOURCE_LEN);
    push_u32(&mut out, RDEF_TYPE_LEN);
    push_u32(&mut out, 12);
    push_u32(&mut out, 0);

    for (buffer, name) in buffers.iter().zip(&buffer_names) {
        let kind = match buffer.register {
            BufferRegister::Srv => 7,
            BufferRegister::Uav => 8,
        };
        push_u32(&mut out, *name);
        push_u32(&mut out, kind);
        // Raw buffers report a mixed return type and the `buffer` dimension.
        push_u32(&mut out, 6);
        push_u32(&mut out, 1);
        push_u32(&mut out, 0);
        push_u32(&mut out, buffer.slot);
        push_u32(&mut out, 1);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
    }
    for name in &constant_buffer_names {
        // Constant buffers are listed among the bound resources as well.
        push_u32(&mut out, *name);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, 1);
        push_u32(&mut out, 1);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
    }
    for (buffer, name) in constant_buffers.iter().zip(&constant_buffer_names) {
        push_u32(&mut out, *name);
        push_u32(&mut out, buffer.variables.len() as u32);
        push_u32(&mut out, variable_offset);
        push_u32(&mut out, buffer.size);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
    }
    for (buffer, names) in constant_buffers.iter().zip(&variable_names) {
        for (variable, name) in buffer.variables.iter().zip(names) {
            push_u32(&mut out, *name);
            push_u32(&mut out, variable.offset);
            push_u32(&mut out, 4);
            push_u32(&mut out, if variable.used { 2 } else { 0 });
            push_u32(&mut out, type_offset);
            push_u32(&mut out, 0);
            push_u32(&mut out, u32::MAX);
            push_u32(&mut out, 0);
            push_u32(&mut out, u32::MAX);
            push_u32(&mut out, 0);
        }
    }

    out.extend_from_slice(&strings.data);

    // Shared scalar `uint` type descriptor: class, type, rows, columns, elements,
    // members, member offset, four unused words, and the type name.
    if variable_count > 0 {
        for value in [19 << 16, 0x0000_0001 | 1 << 16, 0, 0, 0, 0, 0, 0, type_name] {
            push_u32(&mut out, value);
        }
    }

    out.extend_from_slice(reflection.creator.as_bytes());
    out.push(0);
    debug_assert_eq!(out.len(), cursor as usize);
    out
}

/// One input or output signature entry.
pub(crate) struct SignatureElement {
    pub(crate) name: &'static str,
    pub(crate) semantic_index: u32,
    pub(crate) system_value: u32,
    pub(crate) component_type: u32,
    pub(crate) register: u32,
    pub(crate) mask: u8,
    pub(crate) read_mask: u8,
}

const SIGNATURE_ELEMENT_LEN: u32 = 24;

/// Builds an `ISGN` or `OSGN` chunk payload.
pub(crate) fn signature_chunk(elements: &[SignatureElement]) -> Vec<u8> {
    let header = 8u32;
    let mut cursor = header + elements.len() as u32 * SIGNATURE_ELEMENT_LEN;
    let mut strings = Strings { data: Vec::new(), base: cursor };
    let names: Vec<u32> = elements.iter().map(|element| strings.add(element.name)).collect();
    strings.align();
    cursor += strings.data.len() as u32;

    let mut out = Vec::with_capacity(cursor as usize);
    push_u32(&mut out, elements.len() as u32);
    push_u32(&mut out, header);
    for (element, name) in elements.iter().zip(&names) {
        push_u32(&mut out, *name);
        push_u32(&mut out, element.semantic_index);
        push_u32(&mut out, element.system_value);
        push_u32(&mut out, element.component_type);
        push_u32(&mut out, element.register);
        push_u32(&mut out, u32::from(element.mask) | u32::from(element.read_mask) << 8);
    }
    out.extend_from_slice(&strings.data);
    debug_assert_eq!(out.len(), cursor as usize);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(bytes: &[u8]) -> Vec<u32> {
        bytes.chunks_exact(4).map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])).collect()
    }

    fn flatten(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    #[test]
    fn container_records_offsets_and_total_size() {
        let blob = container(&[Chunk::new(*b"RDEF", vec![1, 2, 3, 4]), Chunk::new(*b"SHEX", vec![5, 6, 7, 8])]);
        assert_eq!(&blob[0..4], b"DXBC");
        assert_eq!(u32::from_le_bytes(blob[24..28].try_into().unwrap()) as usize, blob.len());
        assert_eq!(u32::from_le_bytes(blob[28..32].try_into().unwrap()), 2);
        let table = words(&blob[32..40]);
        assert_eq!(table[0] as usize, HEADER_LEN + 8);
        assert_eq!(table[1] as usize, HEADER_LEN + 8 + 8 + 4);
        assert_eq!(&blob[table[0] as usize..table[0] as usize + 4], b"RDEF");
        assert_eq!(&blob[table[1] as usize..table[1] as usize + 4], b"SHEX");
    }

    #[test]
    fn container_digest_covers_the_chunks() {
        let chunks = [Chunk::new(*b"RDEF", vec![1, 2, 3, 4]), Chunk::new(*b"SHEX", vec![5, 6, 7, 8])];
        let blob = container(&chunks);
        assert_ne!(&blob[4..20], &[0u8; 16]);
        assert_eq!(&blob[4..20], &digest(&blob));

        let edited = container(&[Chunk::new(*b"RDEF", vec![1, 2, 3, 4]), Chunk::new(*b"SHEX", vec![5, 6, 7, 9])]);
        assert_ne!(&blob[4..20], &edited[4..20]);
    }

    #[test]
    fn operands_pack_component_selection_and_index_levels() {
        let mut out = Vec::new();
        Operand::temp(0, Select::Mask(0b0001)).encode(&mut out);
        Operand::thread_id(Select::Scalar(0)).encode(&mut out);
        Operand::constant_buffer(0, 0, Select::Scalar(2)).encode(&mut out);
        Operand::immediate(&[0x3f80_0000]).encode(&mut out);
        Operand::null().encode(&mut out);
        assert_eq!(words(&flatten(&out)), vec![0x0010_0012, 0, 0x0002_000a, 0x0030_802a, 0, 0, 0, 0x0000_4001, 0x3f80_0000, 0x0000_d000]);
    }

    #[test]
    fn indexable_temp_reads_relative_index_from_temp() {
        let mut out = Vec::new();
        Operand::indexable_temp(Select::Swizzle([0, 1, 0, 0]), Index::Relative(0, 0)).encode(&mut out);
        assert_eq!(words(&flatten(&out)), vec![0x0420_3046, 0, 0x0010_000a, 0]);
    }

    #[test]
    fn instruction_token_carries_length_and_control() {
        let mut writer = ShaderWriter::new(Stage::Compute, 1);
        writer.refactoring_allowed();
        writer.dcl_temps(1);
        writer.op(Opcode::IfNz, TEST_NONZERO, &[Operand::temp(0, Select::Scalar(0))]);
        let payload = writer.finish();
        assert_eq!(words(&payload[0..8]), vec![0x0005_0051, 8]);
        assert_eq!(words(&payload[8..12]), vec![0x0100_086a]);
        assert_eq!(words(&payload[12..20]), vec![0x0200_0068, 1]);
        assert_eq!(words(&payload[20..32]), vec![0x0304_001f, 0x0010_000a, 0]);
    }

    #[test]
    fn reflection_header_is_self_consistent() {
        let reflection = Reflection {
            stage: Stage::Compute,
            minor: 1,
            creator: "hw".into(),
            constant_buffers: vec![ConstantBuffer {
                name: "Params".into(),
                size: 16,
                variables: vec![Variable { name: "count".into(), offset: 0, used: true }],
            }],
            buffers: vec![BoundBuffer { name: "Out".into(), register: BufferRegister::Uav, slot: 0 }],
        };
        let chunk = reflection_chunk(&reflection);
        let header = words(&chunk[0..0x3c]);
        assert_eq!(header[0], 1);
        assert_eq!(header[1], RDEF_HEADER_LEN + 2 * RDEF_RESOURCE_LEN);
        assert_eq!(header[2], 2);
        assert_eq!(header[3], RDEF_HEADER_LEN);
        assert_eq!(header[6] as usize + reflection.creator.len() + 1, chunk.len());
        assert!(chunk[header[6] as usize..].starts_with(b"hw\0"));

        let constant_buffer = words(&chunk[header[1] as usize..]);
        assert_eq!(constant_buffer[1], 1);
        assert_eq!(constant_buffer[2], header[1] + RDEF_CONSTANT_BUFFER_LEN);
        assert_eq!(constant_buffer[3], 16);
        let variable = words(&chunk[chunk.len() - 4..]);
        assert_eq!(variable.len(), 1);
    }

    #[test]
    fn signature_lists_names_after_elements() {
        let chunk = signature_chunk(&[
            SignatureElement {
                name: "SV_Position",
                semantic_index: 0,
                system_value: 1,
                component_type: 3,
                register: 0,
                mask: 0x0f,
                read_mask: 0,
            },
            SignatureElement { name: "COLOR", semantic_index: 0, system_value: 0, component_type: 3, register: 1, mask: 0x0f, read_mask: 0x0f },
        ]);
        assert_eq!(words(&chunk[0..8]), vec![2, 8]);
        let first_name = words(&chunk[8..12])[0] as usize;
        assert_eq!(first_name, 8 + 2 * SIGNATURE_ELEMENT_LEN as usize);
        assert_eq!(&chunk[first_name..first_name + 11], b"SV_Position");
        assert_eq!(words(&chunk[8..32])[5], 0x0f);
        assert_eq!(words(&chunk[32..56])[5], 0x0f0f);
    }
}
