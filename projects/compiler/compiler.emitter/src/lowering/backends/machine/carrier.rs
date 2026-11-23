//! Node-API / CPython 宿主承载服务（target-lane 私有）。
//!
//! 宿主扩展的包装体构造（注册入口、方法回调、模块定义与 `.data` 布局、文本
//! arena 合同、宿主导入面）是**目标侧承载能力**：语言适配器只提交已冻结的
//! 导出事实（[`HostProperty`] / [`HostMethod`]：符号、整数形参个数、类型化返回
//! 载体）与模块名，不各自复制一套宿主 ABI。
//!
//! 本模块**不要求任何特定函数名**：残差语言把 `main` 暴露成属性 `value` 是
//! 适配器自己的约定，以 [`HostProperty`] 提交；只有方法导出的语言（例如正式
//! 编译 lane 的托管导出面）可以不提交任何属性。
//!
//! 宿主边界合同（所有权、错误与生命周期）：
//!
//! - Node-API 的 `i64` 以 JS `bigint` 为载体：入参用 `napi_get_value_bigint_int64`
//!   并把 `lossless` 出参当失败通道（`false` 即超出 `i64` 域，抛 `RangeError`），
//!   返回值用 `napi_create_bigint_int64`，数组元素共用同一规则。JS `number`
//!   不参与 `i64` 编组：它只有 53 位安全整数，且本 lane 的文本 LIR 没有双精度
//!   范围/小数检查能力；为拒绝静默截断，`number` 实参一律抛 `TypeError`
//!   （见 [`node_method_wrapper`] 的错误消息）。
//! - CPython 的 `i64` 走 `PyLong_FromLongLong` / `PyArg_ParseTuple("n")`，同样
//!   是完整 `i64` 域，不经过 32 位 C `long`；
//! - 布尔按宿主原生布尔类型跨越（`napi_get_boolean` / `PyBool_FromLong`），
//!   不与整数混为同一返回类别；
//! - 字符串返回携带镜像内描述符，宿主侧以 `napi_create_string_utf8` 复制成原生
//!   字符串；长度字段是**UTF-8 字节数**（不是 JS 的 UTF-16 码元数），描述符属于
//!   镜像、永不单独释放，不存在跨 CRT 的所有权转移；
//! - 字符串实参与对象实参尚未冻结载体：正式 lane 在写出前结构化拒绝，不在运行期
//!   用别的类型冒充；
//! - 实参解析失败不静默成 `0`：Node 抛 `TypeError`、CPython 原样传播宿主异常，
//!   `PyModuleDef_Init` / 装箱失败按宿主错误返回。

use super::{
    X64DataTarget, X64ProgramFunction, X64TextInstruction,
    array_runtime::{self, ArrayAllocationSlots, ArrayAllocator},
    host::X64Platform,
};

/// 宿主边界值种类：每个宿主闭合的编组能力在这里显式建模。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostValueKind {
    /// 完整 `i64` 域整数；Node-API 侧以 JS `bigint` 承载。
    Int64,
    /// 原生布尔。
    Bool,
    /// 镜像字符串描述符 → 宿主字符串（UTF-8 字节长度）。
    Utf8,
    /// `[i64]` 堆数组 → 宿主序列（逐元素 `bigint`）。
    ///
    /// guest 返回值归语言值图（native lane 没有回收通道）：桥在调用返回后按
    /// 数组头 `[0]` 逐元素复制到宿主序列，**不释放** guest 数组。返回数组若与
    /// 某个实参镜像别名，桥先完成复制、再释放镜像副本，因此宿主看到的是稳定副本。
    Int64Array,
    /// 无返回值（宿主得到 `undefined` / `None`）。
    Unit,
}

/// 宿主边界实参种类。
///
/// 目前只闭合两类无损实参载体：完整 `i64`（Node-API 以 `bigint` 承载）与
/// `[i64]`（宿主序列）。数组按数组运行时合同的堆表示编组（`[0]` 元素个数、
/// `[8]` 分配字节数、元素从 `+16` 起按 8 字节槽排列）；调用结束后把元素写回
/// 宿主数组，因此原地修改对宿主可见。
///
/// 可变数组合同（`[i64]`）：
///
/// - **实参复制**：调用开始时按宿主数组长度逐元素复制到镜像侧堆数组，宿主数组
///   与镜像副本互不共享存储；
/// - **别名**：同一个 JS 数组作为多个形参传入时，每个形参各得到一份独立副本；
///   调用成功后按形参顺序逐一写回，后写副本整段覆盖先写副本，宿主不得依赖
///   跨形参的同一性；
/// - **写回时点**：只有 guest 调用成功后，才在返回前统一写回全部元素；参数
///   编组失败不会调用 guest，也不会写回任何元素；
/// - **失败清理**：本次调用已分配的镜像副本在抛出宿主错误前逐个释放；部分
///   编组失败不泄漏、不写回半成品；
/// - **生命周期**：镜像侧副本只在本次宿主调用内有效——包装体在写回后立即释放
///   它。guest 若把该副本保存到调用之外，属于违反宿主边界合同（native lane
///   没有逃逸分析，释放责任固定在包装体一侧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostParameterKind {
    /// 完整 `i64` 域整数；Node-API 侧只接受 JS `bigint`。
    Int64,
    /// `[i64]` 宿主数组（逐元素读取，调用后逐元素写回）。
    Int64Array,
    /// `utf8` 文本实参：宿主字符串 → 镜像侧 `Utf8Text` 对象。
    ///
    /// 桥按 UTF-8 字节构造 `{ [0] = _repr 数组 }`（数组每字节占一个 8 字节槽，
    /// 与编译期文本常量同一载体），字节内容复制后与宿主字符串互不共享。对象与
    /// 字节数组都是本次调用的**桥侧临时副本**，返回前释放；guest 把它们保存到
    /// 调用之外属于违反宿主边界合同。
    Utf8,
}

/// 一个模块级属性导出：宿主读取 `<module>.<name>` 时得到 `symbol` 的返回值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProperty {
    /// 宿主可见属性名。
    pub name: String,
    /// 镜像内零参符号名。
    pub symbol: String,
    /// 返回载体。
    pub returns: HostValueKind,
}

/// 一个可暴露给宿主扩展模块的方法（适配器提交的冻结导出事实）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMethod {
    /// 宿主可见方法名（注册字符串与长度按本字段编组）。
    pub name: String,
    /// 镜像内被调用符号名：包装体 `call` 的目标。
    ///
    /// 与 [`Self::name`] 分离，使语言适配器可以暴露任意导出名（例如限定名），
    /// 由适配器自己给出已冻结的镜像符号，承载服务不再假设两者同名。
    pub symbol: String,
    /// 形参载体序列（按位置一一对应）。
    pub parameters: Vec<HostParameterKind>,
    /// 返回载体。
    pub returns: HostValueKind,
}

/// CPython 多阶段模块定义（`PyModuleDef`）头部在 `.data` 中的镜像长度。
///
/// x86-64 布局（全部 8 字节字段）：
///
/// - `0..40`：`PyModuleDef_Base { ob_refcnt = 1, ob_type = NULL, m_init = NULL,
///   m_index = 0, m_copy = NULL }`（即 `PyModuleDef_HEAD_INIT`）；
/// - `40`：`m_name`，`R_X86_64_RELATIVE` 指向紧随其后的模块名；
/// - `48`：`m_doc = NULL`；
/// - `56`：`m_size = 0`（多阶段初始化，模块状态由宿主管理）；
/// - `64`：`m_methods`，有方法时指向 NUL 结尾的 `PyMethodDef` 数组；
/// - `72..104`：`m_slots` 指向 `Py_mod_exec` 槽表，其余为 `NULL`；
/// - `104` 起：模块名、各属性名、各方法名与 `PyArg_ParseTuple` 格式串、方法表。
const PY_MODULE_DEF_BASE_LEN: u32 = 104;

/// `PyMethodDef` 每项 32 字节：`ml_name`、`ml_meth`、`ml_flags`、`ml_doc`。
const PY_METHOD_DEF_LEN: u32 = 32;

/// `METH_VARARGS`：方法以 `(self, args)` 调用，实参由 `PyArg_ParseTuple` 解析。
const PY_METH_VARARGS: u32 = 0x0001;

/// `Py_mod_exec` 槽位编号（`{0, NULL}` 是终止项，合法槽位从 1 起）。
const PY_MOD_EXEC: i32 = 2;

/// 多阶段执行钩子的符号名（`PyInit_` 之外的第二入口）。
pub const PYTHON_EXEC_NAME: &str = "__vm_py_exec";

/// Node-API 回调最多 6 个整数形参：与 SysV64 的整数实参寄存器数一致，
/// `napi_get_cb_info` 的 `argv` 数组按该容量预置。
const NODE_METHOD_MAX_PARAMETERS: usize = 6;

/// 镜像文本 arena 容量：运行期构造的字符串存放在 `.data` 的这段载荷里。
///
/// 所有权归镜像；扩展模块的入口包装体在每次宿主调用开始时把游标归零，
/// 因此单个镜像的峰值占用有界。预算耗尽按 `ud2` 陷入失败闭合。
pub const X64_TEXT_ARENA_CAPACITY: u32 = 64 * 1024;

/// 在 `.data` 末尾追加文本 arena：8 字节游标 + [`X64_TEXT_ARENA_CAPACITY`] 字节载荷。
///
/// 返回游标偏移；`arena_offset + 8` 是载荷起点，游标值按载荷起点相对编码。
pub fn append_text_arena(data: &mut Vec<u8>) -> u32 {
    let offset = u32::try_from(data.len()).expect("native 数据段长度超出 u32");
    data.extend_from_slice(&0u64.to_le_bytes());
    data.resize(data.len() + X64_TEXT_ARENA_CAPACITY as usize, 0);
    offset
}

/// 入口处重置 arena 游标：`[arena_offset] = 0`。
///
/// `reset_slot` 是包装体可用的临时帧槽；必须在调用任何被包装符号之前执行。
pub fn reset_text_arena(reset_slot: u16, arena_offset: u32) -> Vec<X64TextInstruction> {
    use X64TextInstruction as Lir;
    vec![Lir::LoadDataAddress(arena_offset), Lir::StoreSlot(reset_slot), Lir::LoadI64(0), Lir::LoadSlotToRcx(reset_slot), Lir::StoreRaxToRcx]
}

/// 模块名必须是合法 C 标识符：`PyInit_<module>` 要能在 C 中声明。
pub fn is_c_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// 任一导出是否以布尔跨越宿主边界（决定 `napi_get_boolean` / `PyBool_FromLong` 导入）。
pub fn any_returns_bool(properties: &[HostProperty], methods: &[HostMethod]) -> bool {
    properties.iter().any(|property| property.returns == HostValueKind::Bool)
        || methods.iter().any(|method| method.returns == HostValueKind::Bool)
}

/// 任一导出是否返回镜像字符串描述符（决定字符串复制导入）。
pub fn any_returns_utf8(methods: &[HostMethod]) -> bool {
    methods.iter().any(|method| method.returns == HostValueKind::Utf8)
}

/// 任一导出是否返回 `[i64]` 堆数组（决定宿主序列构造导入）。
pub fn any_returns_int64_array(methods: &[HostMethod]) -> bool {
    methods.iter().any(|method| method.returns == HostValueKind::Int64Array)
}

/// 任一导出是否接收 `utf8` 文本实参（决定宿主字符串读取导入）。
pub fn any_takes_utf8(methods: &[HostMethod]) -> bool {
    methods.iter().any(|method| method.parameters.iter().any(|kind| matches!(kind, HostParameterKind::Utf8)))
}

fn python_wrapper_name(method: &str) -> String {
    format!("__nyar_py_wrap_{method}")
}

fn node_wrapper_name(method: &str) -> String {
    format!("__nyar_node_wrap_{method}")
}

/// 追加一个 `PyModuleDef_Slot`（`slot` + 4 字节填充 + `value` 指针槽）。
fn push_module_slot(data: &mut Vec<u8>, slot: i32) {
    data.extend_from_slice(&slot.to_le_bytes());
    data.extend_from_slice(&[0u8; 4]);
    data.extend_from_slice(&[0u8; 8]);
}

fn push_cstring(data: &mut Vec<u8>, value: &str) {
    data.extend_from_slice(value.as_bytes());
    data.push(0);
}

/// 构造 `.data`：多阶段模块定义、模块名、属性名、方法名/格式串、方法表与
/// `PyModuleDef_Slot` 表（`Py_mod_exec`）。
///
/// 多阶段初始化由宿主创建模块并按槽位执行：`PyInit_<module>` 只返回定义地址，
/// 模块实例与每解释器状态由 CPython 管理（`m_size = 0`）。
///
/// 返回数据块、自相对重定位、每个属性的名字偏移与每个方法的格式串偏移（包装体需要）。
pub fn python_module_data(
    module_name: &str,
    properties: &[HostProperty],
    methods: &[HostMethod],
) -> (Vec<u8>, Vec<(u32, X64DataTarget)>, Vec<u32>, Vec<u32>) {
    let mut data = vec![0u8; PY_MODULE_DEF_BASE_LEN as usize];
    let mut relocations = Vec::new();
    data[..8].copy_from_slice(&1i64.to_le_bytes()); // ob_refcnt = 1
    // `m_size = 0`：多阶段初始化，不申请每模块状态。

    let name_offset = data.len() as u32;
    push_cstring(&mut data, module_name);
    relocations.push((40, X64DataTarget::Data(name_offset)));

    let mut property_name_offsets = Vec::with_capacity(properties.len());
    for property in properties {
        property_name_offsets.push(data.len() as u32);
        push_cstring(&mut data, &property.name);
    }

    let mut method_name_offsets = Vec::with_capacity(methods.len());
    let mut format_offsets = Vec::with_capacity(methods.len());
    for method in methods {
        method_name_offsets.push(data.len() as u32);
        push_cstring(&mut data, &method.name);
        // `PyArg_ParseTuple` 的 `n` 是 `Py_ssize_t`（两侧平台都是 8 字节有符号），
        // 与残差的 `i64` 载体一一对应；`utf8` 按 `O` 接收对象后再调
        // `PyUnicode_AsUTF8AndSize` 取字节指针与长度——`#` 系格式要求调用方在
        // 编译期定义 `PY_SSIZE_T_CLEAN`，而本 lane 不链接 Python.h，运行时会直接
        // 拒绝；`O` + 显式长度同时保住内嵌 NUL 与原始字节。`:<name>` 让宿主错误
        // 信息带上方法名。CPython 边界由 `frozen_host_methods` 拒绝数组实参/返回。
        let mut conversions = String::new();
        for kind in &method.parameters {
            match kind {
                HostParameterKind::Int64 => conversions.push('n'),
                HostParameterKind::Utf8 => conversions.push('O'),
                HostParameterKind::Int64Array => unreachable!("CPython 边界拒绝数组实参"),
            }
        }
        let format = format!("{conversions}:{}", method.name);
        format_offsets.push(data.len() as u32);
        push_cstring(&mut data, &format);
    }

    if !methods.is_empty() {
        let methods_offset = data.len() as u32;
        // `m_methods` 指向 NUL 结尾的 `PyMethodDef` 数组；多阶段执行时由
        // `_PyModule_ExecDef` 在槽位执行前安装。
        relocations.push((64, X64DataTarget::Data(methods_offset)));
        for (index, method) in methods.iter().enumerate() {
            let entry = data.len() as u32;
            data.extend_from_slice(&[0u8; PY_METHOD_DEF_LEN as usize]);
            relocations.push((entry, X64DataTarget::Data(method_name_offsets[index])));
            relocations.push((entry + 8, X64DataTarget::Function(python_wrapper_name(&method.symbol))));
            data[entry as usize + 16..entry as usize + 20].copy_from_slice(&PY_METH_VARARGS.to_le_bytes());
        }
        data.extend_from_slice(&[0u8; PY_METHOD_DEF_LEN as usize]); // 空终止项
    }

    // `PyModuleDef_Slot { int slot; void *value; }`（16 字节，含 4 字节对齐填充）：
    // `Py_mod_exec`（2）→ `__vm_py_exec`，然后 `{0, NULL}` 终止。
    let slots_offset = data.len() as u32;
    push_module_slot(&mut data, PY_MOD_EXEC);
    push_module_slot(&mut data, 0);
    relocations.push((72, X64DataTarget::Data(slots_offset)));
    relocations.push((slots_offset + 8, X64DataTarget::Function(PYTHON_EXEC_NAME.to_string())));

    (data, relocations, property_name_offsets, format_offsets)
}

/// `PyObject* wrapper(PyObject* self, PyObject* args)`：以 `PyArg_ParseTuple` 解析
/// 整数实参并调用同名符号，结果按类型化载体跨越边界：
/// `Int64 → PyLong_FromLongLong`、`Bool → PyBool_FromLong`、`Utf8 → PyUnicode_FromStringAndSize`。
///
/// 整数必须使用 64 位装箱：Windows 的 C `long` 是 32 位，`PyLong_FromLong` 会把
/// `4294967297` 静默截成 `1`。
///
/// `PyArg_ParseTuple` 返回 0 表示解析失败（类型/个数不符），此时宿主已经设置异常，
/// 包装体原样返回 `NULL`，不伪造结果。
pub fn python_method_wrapper(method: &HostMethod, format_offset: u32, dll: &str, arena: Option<u32>, platform: X64Platform) -> X64ProgramFunction {
    use X64TextInstruction as Lir;
    const ARGS_SLOT: u16 = 0;
    const FORMAT_SLOT: u16 = 1;
    const TEMP_SLOT: u16 = 2;
    const FIRST_ARGUMENT_SLOT: u16 = 3;
    const FAIL_LABEL: u32 = 0;
    const GATHER_LOOP_LABEL: u32 = 1;
    const GATHER_END_LABEL: u32 = 2;
    const ALLOC_FAIL_LABEL: u32 = 3;
    // 文本实参编组失败：先释放已建立的临时副本，再按 SystemError 通道返回 NULL。
    const PARAM_ALLOC_FAIL_LABEL: u32 = 4;
    // 每个形参占用的转换槽：`n` 一个（Py_ssize_t），`O` 一个（PyObject*）。
    let mut parameter_slots = Vec::with_capacity(method.parameters.len());
    let mut next_slot = FIRST_ARGUMENT_SLOT;
    for kind in &method.parameters {
        parameter_slots.push(next_slot);
        next_slot += match kind {
            HostParameterKind::Int64 => 1,
            HostParameterKind::Utf8 => 1,
            HostParameterKind::Int64Array => 1,
        };
    }
    let text_param_count = method.parameters.iter().filter(|kind| matches!(kind, HostParameterKind::Utf8)).count();
    let text_param_ordinal =
        |index: usize| method.parameters[..index].iter().filter(|kind| matches!(kind, HostParameterKind::Utf8)).count() as u16;
    let parameter_text_base = next_slot;
    let text_param_slot = |index: usize, offset: u16| parameter_text_base + 16 * text_param_ordinal(index) + offset;
    const PP_ARRAY: u16 = 0;
    const PP_ARRAY_COUNT: u16 = 1;
    const PP_ARRAY_BYTES: u16 = 2;
    const PP_ARRAY_AUX: u16 = 3;
    const PP_ARRAY_COND: u16 = 4;
    const PP_ARRAY_FLAG: u16 = 5;
    const PP_OBJECT: u16 = 6;
    const PP_OBJECT_COUNT: u16 = 7;
    const PP_OBJECT_BYTES: u16 = 8;
    const PP_OBJECT_AUX: u16 = 9;
    const PP_OBJECT_COND: u16 = 10;
    const PP_OBJECT_FLAG: u16 = 11;
    const PP_INDEX: u16 = 12;
    const PP_ELEMENT: u16 = 13;
    const PP_CURSOR: u16 = 14;
    // `PyUnicode_AsUTF8AndSize` 写回的字节长度（`Py_ssize_t`）。
    const PP_LEN: u16 = 15;
    // 文本 gather 帧槽：数组指针、计数、分配字节数、分配辅助、条件、暂存缓冲、
    // 游标、下标、字节周转。
    let text_slot_base = parameter_text_base + 16 * text_param_count as u16;
    let text_slot = |offset: u16| text_slot_base + offset;
    let allocator = array_runtime::ArrayAllocator::for_platform(platform);
    // 编组好的宿主对象先落独立槽：释放文本临时副本后再统一返回。
    let result_slot = text_slot_base + if method.returns == HostValueKind::Utf8 { 10 } else { 0 };
    let slot_count = result_slot + 1;
    let emit_free_text_temporaries = |instructions: &mut Vec<Lir>, seed: u32| {
        for (index, kind) in method.parameters.iter().enumerate() {
            if !matches!(kind, HostParameterKind::Utf8) {
                continue;
            }
            let ordinal = text_param_ordinal(index);
            let allocations = [
                (
                    text_param_slot(index, PP_ARRAY_FLAG),
                    text_param_slot(index, PP_ARRAY),
                    ArrayAllocationSlots {
                        count: text_param_slot(index, PP_ARRAY_COUNT),
                        bytes: text_param_slot(index, PP_ARRAY_BYTES),
                        auxiliary: text_param_slot(index, PP_ARRAY_AUX),
                        condition: text_param_slot(index, PP_ARRAY_COND),
                    },
                ),
                (
                    text_param_slot(index, PP_OBJECT_FLAG),
                    text_param_slot(index, PP_OBJECT),
                    ArrayAllocationSlots {
                        count: text_param_slot(index, PP_OBJECT_COUNT),
                        bytes: text_param_slot(index, PP_OBJECT_BYTES),
                        auxiliary: text_param_slot(index, PP_OBJECT_AUX),
                        condition: text_param_slot(index, PP_OBJECT_COND),
                    },
                ),
            ];
            for (slot, (flag, pointer, allocation)) in allocations.into_iter().enumerate() {
                let skip = seed + u32::from(ordinal) * 2 + slot as u32;
                instructions.push(Lir::LoadSlot(flag));
                instructions.push(Lir::JumpIfZero(skip));
                instructions.push(Lir::LoadI64(0));
                instructions.push(Lir::StoreSlot(text_param_slot(index, PP_ELEMENT)));
                instructions.push(Lir::LoadSlot(pointer));
                instructions.extend(array_runtime::array_free(allocator, allocation, text_param_slot(index, PP_ELEMENT)));
                instructions.push(Lir::Label(skip));
            }
        }
    };

    let mut instructions = Vec::new();
    if let Some(arena) = arena {
        instructions.extend(reset_text_arena(TEMP_SLOT, arena));
    }
    instructions.extend([
        // 保存 `args`（self 不参与残差语义）。
        Lir::LoadParam { index: 1, slot: ARGS_SLOT },
        Lir::LoadSlotToArg { slot: ARGS_SLOT, index: 0 },
        Lir::LoadDataAddress(format_offset),
        Lir::StoreSlot(FORMAT_SLOT),
        Lir::LoadSlotToArg { slot: FORMAT_SLOT, index: 1 },
    ]);
    let mut conversion_index = 2u8;
    for (index, kind) in method.parameters.iter().enumerate() {
        // `PyArg_ParseTuple` 按转换顺序写入出参地址。
        let slots = match kind {
            HostParameterKind::Int64 | HostParameterKind::Utf8 => 1,
            HostParameterKind::Int64Array => 0,
        };
        for offset in 0..slots {
            instructions.push(Lir::LoadSlotAddress(parameter_slots[index] + offset));
            instructions.push(Lir::StoreSlot(TEMP_SLOT));
            instructions.push(Lir::LoadSlotToArg { slot: TEMP_SLOT, index: conversion_index });
            conversion_index += 1;
        }
    }
    instructions.push(Lir::CallImport { module: dll.to_string(), field: "PyArg_ParseTuple".to_string() });
    // 返回 0 = 解析失败（异常已由宿主设置）。
    instructions.push(Lir::JumpIfZero(FAIL_LABEL));
    // 文本实参：按 `(char* ptr, Py_ssize_t len)` 建立 `Utf8Text` 对象。
    for (index, kind) in method.parameters.iter().enumerate() {
        if !matches!(kind, HostParameterKind::Utf8) {
            continue;
        }
        let ordinal = text_param_ordinal(index);
        // 标签段避开解析/分配出口（0/3/4）与清理种子（20/40/60）。
        let copy_loop = 100u32 + u32::from(ordinal) * 10;
        let copy_end = copy_loop + 1;
        for flag in [PP_ARRAY_FLAG, PP_OBJECT_FLAG] {
            instructions.push(Lir::LoadI64(0));
            instructions.push(Lir::StoreSlot(text_param_slot(index, flag)));
        }
        instructions.extend([
            // `PyUnicode_AsUTF8AndSize(obj, &len)`：非 `str` 由宿主设置 `TypeError`。
            Lir::LoadSlotToArg { slot: parameter_slots[index], index: 0 },
            Lir::LoadSlotAddress(text_param_slot(index, PP_LEN)),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 1 },
            Lir::CallImport { module: dll.to_string(), field: "PyUnicode_AsUTF8AndSize".to_string() },
            Lir::StoreSlot(text_param_slot(index, PP_CURSOR)),
            Lir::LoadSlot(text_param_slot(index, PP_CURSOR)),
            Lir::JumpIfZero(FAIL_LABEL),
            // 字节数组：count = 字节长度。
            Lir::LoadSlot(text_param_slot(index, PP_LEN)),
        ]);
        instructions.extend(array_runtime::array_allocate(
            allocator,
            ArrayAllocationSlots {
                count: text_param_slot(index, PP_ARRAY_COUNT),
                bytes: text_param_slot(index, PP_ARRAY_BYTES),
                auxiliary: text_param_slot(index, PP_ARRAY_AUX),
                condition: text_param_slot(index, PP_ARRAY_COND),
            },
            PARAM_ALLOC_FAIL_LABEL,
        ));
        instructions.extend([
            Lir::StoreSlot(text_param_slot(index, PP_ARRAY)),
            Lir::LoadI64(1),
            Lir::StoreSlot(text_param_slot(index, PP_ARRAY_FLAG)),
            // 对象块：count = 1，元素区 8 字节存放 `_repr` 指针。
            Lir::LoadI64(1),
        ]);
        instructions.extend(array_runtime::array_allocate(
            allocator,
            ArrayAllocationSlots {
                count: text_param_slot(index, PP_OBJECT_COUNT),
                bytes: text_param_slot(index, PP_OBJECT_BYTES),
                auxiliary: text_param_slot(index, PP_OBJECT_AUX),
                condition: text_param_slot(index, PP_OBJECT_COND),
            },
            PARAM_ALLOC_FAIL_LABEL,
        ));
        instructions.extend([
            Lir::StoreSlot(text_param_slot(index, PP_OBJECT)),
            Lir::LoadI64(1),
            Lir::StoreSlot(text_param_slot(index, PP_OBJECT_FLAG)),
            Lir::LoadSlotToRcx(text_param_slot(index, PP_ARRAY)),
            Lir::LoadSlot(text_param_slot(index, PP_OBJECT)),
            Lir::StoreRcxToRaxDisplacement(array_runtime::ARRAY_ELEMENT_OFFSET as i32),
            // 逐字节分散到 8 字节槽（游标已在取字节指针时写好）。
            Lir::LoadI64(0),
            Lir::StoreSlot(text_param_slot(index, PP_INDEX)),
            Lir::Label(copy_loop),
            Lir::LoadSlot(text_param_slot(index, PP_INDEX)),
            Lir::LoadSlotToRcx(text_param_slot(index, PP_LEN)),
            Lir::Lt,
            Lir::StoreSlot(text_param_slot(index, PP_ARRAY_COND)),
            Lir::LoadSlot(text_param_slot(index, PP_ARRAY_COND)),
            Lir::JumpIfZero(copy_end),
            Lir::LoadSlot(text_param_slot(index, PP_CURSOR)),
            Lir::LoadByteFromRax,
            Lir::StoreSlot(text_param_slot(index, PP_ELEMENT)),
            Lir::LoadSlot(text_param_slot(index, PP_ARRAY)),
            Lir::LoadSlotToRdx(text_param_slot(index, PP_ELEMENT)),
            Lir::LoadSlotToRcx(text_param_slot(index, PP_INDEX)),
            Lir::StoreIndexedRdxToRaxRcx,
            Lir::LoadSlot(text_param_slot(index, PP_CURSOR)),
            Lir::AddRaxImmediate(1),
            Lir::StoreSlot(text_param_slot(index, PP_CURSOR)),
            Lir::LoadSlot(text_param_slot(index, PP_INDEX)),
            Lir::AddRaxImmediate(1),
            Lir::StoreSlot(text_param_slot(index, PP_INDEX)),
            Lir::Jump(copy_loop),
            Lir::Label(copy_end),
        ]);
    }
    for (index, kind) in method.parameters.iter().enumerate() {
        match kind {
            HostParameterKind::Int64 => instructions.push(Lir::LoadSlotToArg { slot: parameter_slots[index], index: index as u8 }),
            HostParameterKind::Utf8 => {
                instructions.push(Lir::LoadSlot(text_param_slot(index, PP_OBJECT)));
                instructions.push(Lir::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET));
                instructions.push(Lir::StoreSlot(TEMP_SLOT));
                instructions.push(Lir::LoadSlotToArg { slot: TEMP_SLOT, index: index as u8 });
            }
            HostParameterKind::Int64Array => unreachable!("CPython 边界拒绝数组实参"),
        }
    }
    instructions.push(Lir::Call(method.symbol.clone()));
    match method.returns {
        HostValueKind::Utf8 => {
            // guest 文本值是 `Utf8Text` 对象：`{ [0] = _repr 数组 }`，数组逐字节占
            // 一个 8 字节槽（数组运行时合同）。桥把字节 gather 进连续暂存缓冲，
            // `PyUnicode_FromStringAndSize` 复制后立即释放；分配失败返回 `NULL`
            // （宿主按 SystemError 通道报告，不伪造结果、不崩溃）。
            instructions.extend([
                Lir::StoreSlot(text_slot(0)),
                Lir::LoadSlot(text_slot(0)),
                Lir::LoadFromRaxDisplacement(0),
                Lir::StoreSlot(text_slot(0)),
                Lir::LoadSlot(text_slot(0)),
                Lir::LoadFromRaxDisplacement(0),
                Lir::StoreSlot(text_slot(1)),
            ]);
            // 暂存缓冲：`array_allocate(count)`。
            instructions.push(Lir::LoadSlot(text_slot(1)));
            instructions.extend(array_runtime::array_allocate(
                allocator,
                ArrayAllocationSlots { count: text_slot(1), bytes: text_slot(2), auxiliary: text_slot(3), condition: text_slot(4) },
                ALLOC_FAIL_LABEL,
            ));
            instructions.extend([
                Lir::StoreSlot(text_slot(5)),
                Lir::LoadI64(0),
                Lir::StoreSlot(text_slot(6)),
                Lir::LoadSlot(text_slot(5)),
                Lir::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET),
                Lir::StoreSlot(text_slot(7)),
                Lir::Label(GATHER_LOOP_LABEL),
                Lir::LoadSlot(text_slot(6)),
                Lir::LoadSlotToRcx(text_slot(1)),
                Lir::Lt,
                Lir::StoreSlot(text_slot(4)),
                Lir::LoadSlot(text_slot(4)),
                Lir::JumpIfZero(GATHER_END_LABEL),
                Lir::LoadSlot(text_slot(0)),
                Lir::LoadSlotToRcx(text_slot(6)),
                Lir::LoadIndexedFromRaxRcx,
                Lir::StoreSlot(text_slot(8)),
                Lir::LoadSlot(text_slot(7)),
                Lir::LoadSlotToRdx(text_slot(8)),
                Lir::StoreByteAtRaxFromRdx,
                Lir::LoadSlot(text_slot(7)),
                Lir::AddRaxImmediate(1),
                Lir::StoreSlot(text_slot(7)),
                Lir::LoadSlot(text_slot(6)),
                Lir::AddRaxImmediate(1),
                Lir::StoreSlot(text_slot(6)),
                Lir::Jump(GATHER_LOOP_LABEL),
                Lir::Label(GATHER_END_LABEL),
                Lir::LoadSlot(text_slot(5)),
                Lir::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET),
                Lir::StoreSlot(text_slot(8)),
                Lir::LoadSlotToArg { slot: text_slot(8), index: 0 },
                Lir::LoadSlotToArg { slot: text_slot(1), index: 1 },
                Lir::CallImport { module: dll.to_string(), field: "PyUnicode_FromStringAndSize".to_string() },
                // 复制已完成：先保存 `PyObject*`，释放暂存缓冲，再返回。
                Lir::StoreSlot(text_slot(8)),
                Lir::LoadI64(0),
                Lir::StoreSlot(text_slot(9)),
                Lir::LoadSlot(text_slot(5)),
            ]);
            instructions.extend(array_runtime::array_free(
                allocator,
                ArrayAllocationSlots { count: text_slot(1), bytes: text_slot(2), auxiliary: text_slot(3), condition: text_slot(4) },
                text_slot(9),
            ));
            instructions.extend([Lir::LoadSlot(text_slot(8)), Lir::StoreSlot(result_slot)]);
        }
        HostValueKind::Bool => {
            instructions.push(Lir::StoreSlot(TEMP_SLOT));
            instructions.push(Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 0 });
            instructions.push(Lir::CallImport { module: dll.to_string(), field: "PyBool_FromLong".to_string() });
            instructions.push(Lir::StoreSlot(result_slot));
        }
        HostValueKind::Int64 => {
            instructions.push(Lir::StoreSlot(TEMP_SLOT));
            instructions.push(Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 0 });
            instructions.push(Lir::CallImport { module: dll.to_string(), field: "PyLong_FromLongLong".to_string() });
            instructions.push(Lir::StoreSlot(result_slot));
        }
        HostValueKind::Int64Array => unreachable!("CPython 边界由 frozen_host_methods 拒绝数组返回"),
        HostValueKind::Unit => unreachable!("CPython 边界由 frozen_host_methods 拒绝 unit 返回"),
    }
    // 宿主对象已经建立：释放文本实参的临时副本，再交出结果。
    emit_free_text_temporaries(&mut instructions, 20);
    instructions.extend([Lir::LoadSlot(result_slot), Lir::Return]);
    // 解析失败：宿主已设置异常，直接返回 `NULL`，此时没有任何编组分配。
    instructions.push(Lir::Label(FAIL_LABEL));
    instructions.push(Lir::LoadI64(0));
    instructions.push(Lir::Return);
    // 文本实参编组失败：先释放已建立的临时副本。
    instructions.push(Lir::Label(PARAM_ALLOC_FAIL_LABEL));
    emit_free_text_temporaries(&mut instructions, 40);
    instructions.push(Lir::LoadI64(0));
    instructions.push(Lir::Return);
    // 返回文本的暂存缓冲分配失败：释放实参临时副本后按 SystemError 通道返回 `NULL`。
    instructions.push(Lir::Label(ALLOC_FAIL_LABEL));
    emit_free_text_temporaries(&mut instructions, 60);
    instructions.push(Lir::LoadI64(0));
    instructions.push(Lir::Return);

    X64ProgramFunction { name: python_wrapper_name(&method.symbol), slot_count, instructions }
}

/// `PyInit_<module>` 入口：多阶段初始化返回经 `PyModuleDef_Init` 初始化的定义地址。
///
/// CPython 的导入器用 `PyObject_TypeCheck(m, &PyModuleDef_Type)` 判定多阶段模块，
/// 因此定义必须先由 `PyModuleDef_Init` 设置 `ob_type`；随后宿主调用
/// `PyModule_FromDefAndSpec2` 创建模块并执行 `Py_mod_exec` 槽位。
pub fn python_module_registration(module_name: &str, dll: &str) -> X64ProgramFunction {
    use X64TextInstruction as Lir;
    X64ProgramFunction {
        name: format!("PyInit_{module_name}"),
        slot_count: 1,
        instructions: vec![
            Lir::LoadDataAddress(0),
            Lir::StoreSlot(0),
            Lir::LoadSlotToArg { slot: 0, index: 0 },
            Lir::CallImport { module: dll.to_string(), field: "PyModuleDef_Init".to_string() },
            Lir::Return,
        ],
    }
}

/// `int __vm_py_exec(PyObject* module)`：`Py_mod_exec` 槽位的执行体。
///
/// 模块实例由宿主创建；这里逐个安装属性（方法表由 `_PyModule_ExecDef` 依据
/// `m_methods` 先行安装）。成功返回 0，失败返回 -1，不吞掉宿主错误。
///
/// `Int64` 先经 `PyLong_FromLongLong` 装箱再交给 `PyModule_AddObject`：
/// `PyModule_AddIntConstant` 走 C `long`，在 Windows 上只有 32 位，会把 i64 截断；
/// `Bool` 经 `PyBool_FromLong` 得到原生 `bool`，不与整数混为同一返回类别。
pub fn python_module_exec(properties: &[HostProperty], property_name_offsets: &[u32], dll: &str, arena: Option<u32>) -> X64ProgramFunction {
    use X64TextInstruction as Lir;
    const MODULE_SLOT: u16 = 0;
    const VALUE_SLOT: u16 = 1;
    const OBJECT_SLOT: u16 = 2;
    const RESULT_SLOT: u16 = 3;
    const ZERO_SLOT: u16 = 4;
    const TEMP_SLOT: u16 = 5;
    const FAIL_LABEL: u32 = 0;
    let mut instructions = vec![Lir::LoadParam { index: 0, slot: MODULE_SLOT }];
    if let Some(arena) = arena {
        // 入口实参已落槽：重置使用 rax/rcx 不会破坏 Win64 的 rcx 形参。
        instructions.extend(reset_text_arena(TEMP_SLOT, arena));
    }
    instructions.push(Lir::LoadI64(0));
    instructions.push(Lir::StoreSlot(ZERO_SLOT));
    for (property, name_offset) in properties.iter().zip(property_name_offsets) {
        instructions.extend([
            Lir::Call(property.symbol.clone()),
            Lir::StoreSlot(VALUE_SLOT),
            // 装箱：整数走 64 位有符号，布尔走原生 `bool`。
            Lir::LoadSlotToArg { slot: VALUE_SLOT, index: 0 },
            Lir::CallImport {
                module: dll.to_string(),
                field: match property.returns {
                    HostValueKind::Bool => "PyBool_FromLong",
                    _ => "PyLong_FromLongLong",
                }
                .to_string(),
            },
            Lir::StoreSlot(OBJECT_SLOT),
            // `NULL` 表示装箱失败（异常已由宿主设置）。
            Lir::LoadSlot(OBJECT_SLOT),
            Lir::LoadSlotToRcx(ZERO_SLOT),
            Lir::Ne,
            Lir::JumpIfZero(FAIL_LABEL),
            // `PyModule_AddObject(module, name, object)`：成功时接管对象引用。
            Lir::LoadSlotToArg { slot: MODULE_SLOT, index: 0 },
            Lir::LoadDataAddress(*name_offset),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 1 },
            Lir::LoadSlotToArg { slot: OBJECT_SLOT, index: 2 },
            Lir::CallImport { module: dll.to_string(), field: "PyModule_AddObject".to_string() },
            Lir::StoreSlot(RESULT_SLOT),
            Lir::LoadSlot(RESULT_SLOT),
            Lir::LoadSlotToRcx(ZERO_SLOT),
            Lir::Eq,
            Lir::JumpIfZero(FAIL_LABEL),
        ]);
    }
    instructions.extend([Lir::LoadI64(0), Lir::Return, Lir::Label(FAIL_LABEL), Lir::LoadI64(-1), Lir::Return]);
    X64ProgramFunction { name: PYTHON_EXEC_NAME.to_string(), slot_count: 6, instructions }
}

/// Node-API 数据段：属性名、错误消息与各方法名（全部 NUL 结尾）。
///
/// 返回 `(数据块, 属性名偏移, 方法名偏移, 类型错误消息偏移, 越界错误消息偏移,
/// 数组分配失败消息偏移)`。三条消息分开：类型不符、`bigint` 超出 `i64` 域与
/// 资源失败不是同一个宿主错误类别。
pub fn node_module_data(
    properties: &[HostProperty],
    methods: &[HostMethod],
) -> (Vec<u8>, Vec<u32>, Vec<u32>, u32, u32, u32) {
    let mut data = Vec::new();
    let mut property_name_offsets = Vec::with_capacity(properties.len());
    for property in properties {
        property_name_offsets.push(data.len() as u32);
        push_cstring(&mut data, &property.name);
    }
    // 类型错误：Node-API 侧 `i64` 的唯一载体是 `bigint`；`number` 只有 53 位安全
    // 整数，接受它就必须先做双精度范围与小数检查，而当前 LIR 没有该能力。宁可
    // 显式拒绝，也不静默截断成 32 位或丢精度。
    let error_message_offset = data.len() as u32;
    push_cstring(
        &mut data,
        "expected bigint arguments: i64 crosses the Node-API boundary as BigInt; JS number is rejected to avoid precision loss",
    );
    // `napi_get_value_bigint_int64` 的 `lossless` 为 false：值超出 `i64` 域。
    let range_error_offset = data.len() as u32;
    push_cstring(&mut data, "bigint argument out of i64 range");
    let allocation_error_offset = data.len() as u32;
    push_cstring(&mut data, "array allocation failed");
    let mut method_name_offsets = Vec::with_capacity(methods.len());
    for method in methods {
        method_name_offsets.push(data.len() as u32);
        push_cstring(&mut data, &method.name);
    }
    (data, property_name_offsets, method_name_offsets, error_message_offset, range_error_offset, allocation_error_offset)
}

/// `napi_register_module_v1(env, exports)` 入口：把每个属性的零参符号结果写成
/// `exports.<name>`，并为每个方法注册同名回调，最后返回 `exports` 对象。
///
/// 属性集合由适配器提交：残差语言可以暴露 `{ value }`，正式编译 lane 可以只
/// 暴露方法；本模块不要求任何特定符号名。
pub fn node_addon_registration(
    properties: &[HostProperty],
    property_name_offsets: &[u32],
    methods: &[HostMethod],
    method_name_offsets: &[u32],
    platform: X64Platform,
    dll: &str,
    arena: Option<u32>,
) -> X64ProgramFunction {
    use X64TextInstruction as Lir;
    const ENV_SLOT: u16 = 0;
    const EXPORTS_SLOT: u16 = 1;
    const VALUE_SLOT: u16 = 2;
    const TEMP_SLOT: u16 = 3;
    const FUNCTION_SLOT: u16 = 4;
    const ZERO_SLOT: u16 = 5;
    const OUT_SLOT: u16 = 6;

    let mut instructions = vec![
        Lir::LoadParam { index: 0, slot: ENV_SLOT },
        Lir::LoadParam { index: 1, slot: EXPORTS_SLOT },
        Lir::LoadI64(0),
        Lir::StoreSlot(ZERO_SLOT),
    ];
    if let Some(arena) = arena {
        instructions.extend(reset_text_arena(TEMP_SLOT, arena));
    }
    for (property, name_offset) in properties.iter().zip(property_name_offsets) {
        instructions.extend([
            Lir::Call(property.symbol.clone()),
            Lir::StoreSlot(VALUE_SLOT),
            // `napi_create_bigint_int64(env, value, &out)` / `napi_get_boolean(env, value, &out)`。
            // `i64` 属性与返回值共用 bigint 载体，避免 53 位以上的值在导出时丢精度。
            Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
            Lir::LoadSlotToArg { slot: VALUE_SLOT, index: 1 },
            Lir::LoadSlotAddress(OUT_SLOT),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
            Lir::CallImport {
                module: dll.to_string(),
                field: match property.returns {
                    HostValueKind::Bool => "napi_get_boolean",
                    _ => "napi_create_bigint_int64",
                }
                .to_string(),
            },
            // `napi_set_named_property(env, exports, name, out)`。
            Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
            Lir::LoadSlotToArg { slot: EXPORTS_SLOT, index: 1 },
            Lir::LoadDataAddress(*name_offset),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
            Lir::LoadSlotToArg { slot: OUT_SLOT, index: 3 },
            Lir::CallImport { module: dll.to_string(), field: "napi_set_named_property".to_string() },
        ]);
    }
    for (method, name_offset) in methods.iter().zip(method_name_offsets) {
        // `napi_create_function(env, name, len, wrapper, data, &fn)`。
        instructions.extend([
            Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
            Lir::LoadDataAddress(*name_offset),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 1 },
            Lir::LoadI64(method.name.len() as i64),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
            Lir::LoadFunctionAddress(node_wrapper_name(&method.symbol)),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 3 },
        ]);
        if platform.is_windows() {
            // 第 5、6 个实参走 Win64 栈：`data = NULL`、`result = &fn`。
            instructions.push(Lir::ZeroStackArg(0));
            instructions.push(Lir::StoreSlotAddressToStackArg { slot: FUNCTION_SLOT, index: 1 });
        }
        else {
            instructions.push(Lir::LoadSlotToArg { slot: ZERO_SLOT, index: 4 });
            instructions.push(Lir::LoadSlotAddress(FUNCTION_SLOT));
            instructions.push(Lir::StoreSlot(TEMP_SLOT));
            instructions.push(Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 5 });
        }
        instructions.extend([
            Lir::CallImport { module: dll.to_string(), field: "napi_create_function".to_string() },
            // `napi_set_named_property(env, exports, name, fn)`。
            Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
            Lir::LoadSlotToArg { slot: EXPORTS_SLOT, index: 1 },
            Lir::LoadDataAddress(*name_offset),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
            Lir::LoadSlotToArg { slot: FUNCTION_SLOT, index: 3 },
            Lir::CallImport { module: dll.to_string(), field: "napi_set_named_property".to_string() },
        ]);
    }
    instructions.push(Lir::LoadSlot(EXPORTS_SLOT));
    instructions.push(Lir::Return);

    X64ProgramFunction { name: "napi_register_module_v1".to_string(), slot_count: 7, instructions }
}

/// `napi_value wrapper(napi_env env, napi_callback_info info)`：把 Node 实参解析成
/// `i64` 后调用同名符号，结果按类型化载体跨越边界：
/// `Int64 → napi_create_bigint_int64`（完整 i64 域，以 JS `bigint` 承载）、
/// `Bool → napi_get_boolean`、`Utf8 → napi_create_string_utf8`（UTF-8 字节长度）。
///
/// `i64` 入参只接受 JS `bigint`：`napi_get_value_bigint_int64` 的 `lossless` 为
/// false 时抛 `RangeError`；非 `bigint`（包括 `number`）抛 `TypeError`。参数个数
/// 不符同样抛 `TypeError`，不把错误静默成 0。数百万个 `i64` 值不会经过 JS
/// `number`，因此不存在 53 位精度上限。
///
/// 数组合同见 [`HostParameterKind::Int64Array`]：先复制、调用成功后统一写回、
/// 失败时释放本次调用已分配的镜像副本。
pub fn node_method_wrapper(
    method: &HostMethod,
    error_message_offset: u32,
    range_error_offset: u32,
    allocation_error_offset: u32,
    platform: X64Platform,
    dll: &str,
    arena: Option<u32>,
) -> X64ProgramFunction {
    use X64TextInstruction as Lir;
    const ENV_SLOT: u16 = 0;
    const INFO_SLOT: u16 = 1;
    const ARGC_SLOT: u16 = 2;
    const ZERO_SLOT: u16 = 3;
    const TEMP_SLOT: u16 = 4;
    // `argv` 数组占用的帧槽；槽号递增对应地址递减，数组基址取最高槽号。
    const ARGV_FIRST_SLOT: u16 = 5;
    const RESULT_SLOT: u16 = 11;
    const OUT_FIRST_SLOT: u16 = 12;
    // 字符串返回的额外帧槽：`argv`/out 区最多到 17，以下从 18 起，互不重叠。
    const STRING_SLOT: u16 = 21;
    // `napi_get_value_bigint_int64` 的 `bool* lossless` 出参；每次调用前清零。
    const BIGINT_LOSSLESS_SLOT: u16 = 22;
    const FAIL_LABEL: u32 = 0;
    const CALLBACK_OK_LABEL: u32 = 1;
    // 文本 gather 循环标签：避开参数继续标签（`2 + index`）与清理种子（300/400/500）。
    const GATHER_LOOP_LABEL: u32 = 600;
    const GATHER_END_LABEL: u32 = 601;
    let argv_slot = |index: usize| ARGV_FIRST_SLOT + NODE_METHOD_MAX_PARAMETERS as u16 - 1 - index as u16;
    let out_slot = |index: usize| OUT_FIRST_SLOT + index as u16;
    let continue_label = |index: usize| 2 + index as u32;
    let parameters = method.parameters.len();
    // 数组运行时合同按平台身份选择分配/释放实现（Win64 进程堆 / Linux mmap）。
    let allocator = ArrayAllocator::for_platform(platform);
    // 越界与分配失败出口：排在参数继续标签之后，避免与 `2 + index` 或数组循环标签冲突。
    // 类型错误走 `FAIL_LABEL`；这三个出口都会先释放本次调用已分配的镜像副本。
    let range_failure_label = 2 + parameters as u32;
    let allocation_failure_label = 3 + parameters as u32;
    // 每个 `[i64]` 实参占用 9 个互不重叠的帧槽：计数、游标、长度出参、布尔出参、
    // 元素句柄、元素值（分配期间兼作字节数）、循环条件、分配句柄、已分配标记。
    // 槽位按“数组序号”排列，不能按形参下标，否则标量在数组之前时会越出帧大小。
    let array_slot_base = BIGINT_LOSSLESS_SLOT + 1;
    let array_ordinal = |index: usize| method.parameters[..index].iter().filter(|kind| matches!(kind, HostParameterKind::Int64Array)).count() as u16;
    let array_slot = |index: usize, offset: u16| array_slot_base + 9 * array_ordinal(index) + offset;
    const ARRAY_COUNT: u16 = 0;
    const ARRAY_INDEX: u16 = 1;
    const ARRAY_LENGTH: u16 = 2;
    const ARRAY_BOOL: u16 = 3;
    const ARRAY_ELEMENT: u16 = 4;
    const ARRAY_VALUE: u16 = 5;
    const ARRAY_CONDITION: u16 = 6;
    const ARRAY_HEAP: u16 = 7;
    const ARRAY_ALLOCATED: u16 = 8;
    let array_count = method.parameters.iter().filter(|kind| matches!(kind, HostParameterKind::Int64Array)).count();
    // `utf8` 实参的编组区：每个文本实参独立一段，排在数组区之后。
    // 偏移：长度、暂存缓冲指针/计数/字节数/辅助/条件/分配标记、字节数组
    // 指针/计数/字节数/辅助/条件/分配标记、对象块指针/计数/字节数/辅助/条件/
    // 分配标记、下标、元素、字节游标。
    let text_param_count = method.parameters.iter().filter(|kind| matches!(kind, HostParameterKind::Utf8)).count();
    let text_param_base = array_slot_base + 9 * array_count as u16;
    let text_param_ordinal =
        |index: usize| method.parameters[..index].iter().filter(|kind| matches!(kind, HostParameterKind::Utf8)).count() as u16;
    let text_param_slot = |index: usize, offset: u16| text_param_base + 22 * text_param_ordinal(index) + offset;
    const TP_LEN: u16 = 0;
    const TP_SCRATCH: u16 = 1;
    const TP_SCRATCH_COUNT: u16 = 2;
    const TP_SCRATCH_BYTES: u16 = 3;
    const TP_SCRATCH_AUX: u16 = 4;
    const TP_SCRATCH_COND: u16 = 5;
    const TP_SCRATCH_FLAG: u16 = 6;
    const TP_ARRAY: u16 = 7;
    const TP_ARRAY_COUNT: u16 = 8;
    const TP_ARRAY_BYTES: u16 = 9;
    const TP_ARRAY_AUX: u16 = 10;
    const TP_ARRAY_COND: u16 = 11;
    const TP_ARRAY_FLAG: u16 = 12;
    const TP_OBJECT: u16 = 13;
    const TP_OBJECT_COUNT: u16 = 14;
    const TP_OBJECT_BYTES: u16 = 15;
    const TP_OBJECT_AUX: u16 = 16;
    const TP_OBJECT_COND: u16 = 17;
    const TP_OBJECT_FLAG: u16 = 18;
    const TP_INDEX: u16 = 19;
    const TP_ELEMENT: u16 = 20;
    const TP_CURSOR: u16 = 21;
    // 文本返回的 gather 帧槽：数组指针、计数、分配字节数、分配辅助、条件、
    // 暂存缓冲、游标、下标、字节周转——排在文本实参区之后，互不重叠。
    let returns_text = method.returns == HostValueKind::Utf8;
    let text_slot_base = text_param_base + 22 * text_param_count as u16;
    let text_slot = |offset: u16| text_slot_base + offset;
    // `[i64]` 返回的复制区：宿主序列句柄、元素个数、下标、元素值、元素句柄、条件。
    let returns_array = method.returns == HostValueKind::Int64Array;
    let array_return_base = text_slot_base + if returns_text { 9 } else { 0 };
    let array_return_slot = |offset: u16| array_return_base + offset;
    const AR_JS_ARRAY: u16 = 0;
    const AR_COUNT: u16 = 1;
    const AR_INDEX: u16 = 2;
    const AR_VALUE: u16 = 3;
    const AR_ELEMENT: u16 = 4;
    const AR_CONDITION: u16 = 5;
    let slot_count = array_return_base + if returns_array { 6 } else { 0 };

    // 失败清理：只释放本次调用真正分配成功过的镜像副本（标记在分配后置 1），
    // 因此部分编组失败不会泄漏，也不会写回半成品。
    let emit_free_allocated = |instructions: &mut Vec<Lir>, seed: u32| {
        for (index, kind) in method.parameters.iter().enumerate() {
            if !matches!(kind, HostParameterKind::Int64Array) {
                continue;
            }
            let skip = seed + index as u32;
            instructions.push(Lir::LoadSlot(array_slot(index, ARRAY_ALLOCATED)));
            instructions.push(Lir::JumpIfZero(skip));
            instructions.push(Lir::LoadI64(0));
            instructions.push(Lir::StoreSlot(array_slot(index, ARRAY_VALUE)));
            instructions.push(Lir::LoadSlot(out_slot(index)));
            instructions.extend(array_runtime::array_free(
                allocator,
                ArrayAllocationSlots {
                    count: array_slot(index, ARRAY_COUNT),
                    bytes: array_slot(index, ARRAY_VALUE),
                    auxiliary: array_slot(index, ARRAY_HEAP),
                    condition: array_slot(index, ARRAY_CONDITION),
                },
                array_slot(index, ARRAY_VALUE),
            ));
            instructions.push(Lir::Label(skip));
        }
    };
    // 文本实参的桥侧临时副本：暂存缓冲、字节数组、`Utf8Text` 对象块，各自按
    // 分配标记释放。返回与失败出口共用这一段，避免部分编组泄漏。
    let emit_free_text_temporaries = |instructions: &mut Vec<Lir>, seed: u32| {
        for (index, kind) in method.parameters.iter().enumerate() {
            if !matches!(kind, HostParameterKind::Utf8) {
                continue;
            }
            let ordinal = text_param_ordinal(index);
            let allocations = [
                (
                    text_param_slot(index, TP_SCRATCH_FLAG),
                    text_param_slot(index, TP_SCRATCH),
                    ArrayAllocationSlots {
                        count: text_param_slot(index, TP_SCRATCH_COUNT),
                        bytes: text_param_slot(index, TP_SCRATCH_BYTES),
                        auxiliary: text_param_slot(index, TP_SCRATCH_AUX),
                        condition: text_param_slot(index, TP_SCRATCH_COND),
                    },
                ),
                (
                    text_param_slot(index, TP_ARRAY_FLAG),
                    text_param_slot(index, TP_ARRAY),
                    ArrayAllocationSlots {
                        count: text_param_slot(index, TP_ARRAY_COUNT),
                        bytes: text_param_slot(index, TP_ARRAY_BYTES),
                        auxiliary: text_param_slot(index, TP_ARRAY_AUX),
                        condition: text_param_slot(index, TP_ARRAY_COND),
                    },
                ),
                (
                    text_param_slot(index, TP_OBJECT_FLAG),
                    text_param_slot(index, TP_OBJECT),
                    ArrayAllocationSlots {
                        count: text_param_slot(index, TP_OBJECT_COUNT),
                        bytes: text_param_slot(index, TP_OBJECT_BYTES),
                        auxiliary: text_param_slot(index, TP_OBJECT_AUX),
                        condition: text_param_slot(index, TP_OBJECT_COND),
                    },
                ),
            ];
            for (slot, (flag, pointer, allocation)) in allocations.into_iter().enumerate() {
                let skip = seed + u32::from(ordinal) * 3 + slot as u32;
                instructions.push(Lir::LoadSlot(flag));
                instructions.push(Lir::JumpIfZero(skip));
                instructions.push(Lir::LoadI64(0));
                instructions.push(Lir::StoreSlot(text_param_slot(index, TP_ELEMENT)));
                instructions.push(Lir::LoadSlot(pointer));
                instructions.extend(array_runtime::array_free(allocator, allocation, text_param_slot(index, TP_ELEMENT)));
                instructions.push(Lir::Label(skip));
            }
        }
    };

    let mut instructions = vec![
        Lir::LoadParam { index: 0, slot: ENV_SLOT },
        Lir::LoadParam { index: 1, slot: INFO_SLOT },
        // `argc` 是 in/out：传入 `argv` 容量，返回实际个数。
        Lir::LoadI64(NODE_METHOD_MAX_PARAMETERS as i64),
        Lir::StoreSlot(ARGC_SLOT),
        Lir::LoadI64(0),
        Lir::StoreSlot(ZERO_SLOT),
    ];
    // 先清零所有镜像副本的分配标记：失败出口可能在任何分配之前到达。
    for (index, kind) in method.parameters.iter().enumerate() {
        if matches!(kind, HostParameterKind::Int64Array) {
            instructions.push(Lir::LoadI64(0));
            instructions.push(Lir::StoreSlot(array_slot(index, ARRAY_ALLOCATED)));
        }
        if matches!(kind, HostParameterKind::Utf8) {
            for flag in [TP_SCRATCH_FLAG, TP_ARRAY_FLAG, TP_OBJECT_FLAG] {
                instructions.push(Lir::LoadI64(0));
                instructions.push(Lir::StoreSlot(text_param_slot(index, flag)));
            }
        }
    }
    if let Some(arena) = arena {
        instructions.extend(reset_text_arena(TEMP_SLOT, arena));
    }
    instructions.extend([
        // `napi_get_cb_info(env, info, &argc, argv, NULL, NULL)`。
        Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
        Lir::LoadSlotToArg { slot: INFO_SLOT, index: 1 },
        Lir::LoadSlotAddress(ARGC_SLOT),
        Lir::StoreSlot(TEMP_SLOT),
        Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
        Lir::LoadSlotAddress(ARGV_FIRST_SLOT + NODE_METHOD_MAX_PARAMETERS as u16 - 1),
        Lir::StoreSlot(TEMP_SLOT),
        Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 3 },
    ]);
    // `napi_get_cb_info` 的第 5、6 个实参是 `this` 与 `data`，恒为 `NULL`。
    if platform.is_windows() {
        instructions.push(Lir::ZeroStackArg(0));
        instructions.push(Lir::ZeroStackArg(1));
    }
    else {
        instructions.push(Lir::LoadSlotToArg { slot: ZERO_SLOT, index: 4 });
        instructions.push(Lir::LoadSlotToArg { slot: ZERO_SLOT, index: 5 });
    }
    instructions.extend([
        Lir::CallImport { module: dll.to_string(), field: "napi_get_cb_info".to_string() },
        // `napi_get_cb_info` 返回 0 才是成功。
        Lir::JumpIfZero(CALLBACK_OK_LABEL),
        Lir::Jump(FAIL_LABEL),
        Lir::Label(CALLBACK_OK_LABEL),
        // 实际个数必须与方法形参一致。
        Lir::LoadI64(parameters as i64),
        Lir::StoreSlot(TEMP_SLOT),
        Lir::LoadSlot(ARGC_SLOT),
        Lir::LoadSlotToRcx(TEMP_SLOT),
        Lir::Eq,
        Lir::JumpIfZero(FAIL_LABEL),
    ]);
    for (index, kind) in method.parameters.iter().enumerate() {
        match kind {
            HostParameterKind::Int64 => {
                // `napi_get_value_bigint_int64(env, argv[index], &out, &lossless)`：
                // 只接受 JS `bigint`，`lossless == false` 表示超出 i64 域。
                instructions.extend([
                    Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                    Lir::LoadSlotToArg { slot: argv_slot(index), index: 1 },
                    Lir::LoadSlotAddress(out_slot(index)),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
                    Lir::LoadI64(0),
                    Lir::StoreSlot(BIGINT_LOSSLESS_SLOT),
                    Lir::LoadSlotAddress(BIGINT_LOSSLESS_SLOT),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 3 },
                    Lir::CallImport { module: dll.to_string(), field: "napi_get_value_bigint_int64".to_string() },
                    Lir::JumpIfZero(continue_label(index)),
                    Lir::Jump(FAIL_LABEL),
                    Lir::Label(continue_label(index)),
                    Lir::LoadSlotAddress(BIGINT_LOSSLESS_SLOT),
                    Lir::LoadByteFromRax,
                    Lir::JumpIfZero(range_failure_label),
                ]);
            }
            HostParameterKind::Int64Array => {
                // 标签段按实参位置隔离，避免同一包装体内多数组实参互相跳转。
                let label_base = 100 + index as u32 * 10;
                let length_ok = label_base;
                let fill_loop = label_base + 1;
                let fill_end = label_base + 2;
                let element_ok = label_base + 3;
                let integer_ok = label_base + 4;
                instructions.extend([
                    // 必须是宿主数组。
                    Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                    Lir::LoadSlotToArg { slot: argv_slot(index), index: 1 },
                    Lir::LoadSlotAddress(array_slot(index, ARRAY_BOOL)),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
                    Lir::CallImport { module: dll.to_string(), field: "napi_is_array".to_string() },
                    Lir::LoadSlotAddress(array_slot(index, ARRAY_BOOL)),
                    Lir::LoadByteFromRax,
                    Lir::JumpIfZero(FAIL_LABEL),
                    // `napi_get_array_length` 写 `uint32_t`：先清零整个 8 字节槽。
                    Lir::LoadI64(0),
                    Lir::StoreSlot(array_slot(index, ARRAY_LENGTH)),
                    Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                    Lir::LoadSlotToArg { slot: argv_slot(index), index: 1 },
                    Lir::LoadSlotAddress(array_slot(index, ARRAY_LENGTH)),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
                    Lir::CallImport { module: dll.to_string(), field: "napi_get_array_length".to_string() },
                    Lir::JumpIfZero(length_ok),
                    Lir::Jump(FAIL_LABEL),
                    Lir::Label(length_ok),
                    Lir::LoadSlot(array_slot(index, ARRAY_LENGTH)),
                    Lir::StoreSlot(array_slot(index, ARRAY_COUNT)),
                ]);
                // 分配冻结表示：字节数换算、分配实现与失败出口统一来自数组运行时合同。
                instructions.push(Lir::LoadSlot(array_slot(index, ARRAY_COUNT)));
                instructions.extend(array_runtime::array_allocate(
                    allocator,
                    ArrayAllocationSlots {
                        count: array_slot(index, ARRAY_COUNT),
                        bytes: array_slot(index, ARRAY_VALUE),
                        auxiliary: array_slot(index, ARRAY_HEAP),
                        condition: array_slot(index, ARRAY_CONDITION),
                    },
                    allocation_failure_label,
                ));
                instructions.push(Lir::StoreSlot(out_slot(index)));
                // 分配成功后才置标记：失败清理只释放真正分配过的镜像副本。
                instructions.push(Lir::LoadI64(1));
                instructions.push(Lir::StoreSlot(array_slot(index, ARRAY_ALLOCATED)));
                instructions.extend([
                    // `index = 0`。
                    Lir::LoadI64(0),
                    Lir::StoreSlot(array_slot(index, ARRAY_INDEX)),
                    Lir::LoadSlot(array_slot(index, ARRAY_INDEX)),
                    Lir::LoadSlotToRcx(array_slot(index, ARRAY_COUNT)),
                    Lir::Lt,
                    Lir::StoreSlot(array_slot(index, ARRAY_CONDITION)),
                    Lir::LoadSlot(array_slot(index, ARRAY_CONDITION)),
                    Lir::JumpIfZero(fill_end),
                    Lir::Label(fill_loop),
                    // `napi_get_element(env, array, index, &element)`。
                    Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                    Lir::LoadSlotToArg { slot: argv_slot(index), index: 1 },
                    Lir::LoadSlotToArg { slot: array_slot(index, ARRAY_INDEX), index: 2 },
                    Lir::LoadSlotAddress(array_slot(index, ARRAY_ELEMENT)),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 3 },
                    Lir::CallImport { module: dll.to_string(), field: "napi_get_element".to_string() },
                    Lir::JumpIfZero(element_ok),
                    Lir::Jump(FAIL_LABEL),
                    Lir::Label(element_ok),
                    // `napi_get_value_bigint_int64(env, element, &value, &lossless)`。
                    Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                    Lir::LoadSlotToArg { slot: array_slot(index, ARRAY_ELEMENT), index: 1 },
                    Lir::LoadSlotAddress(array_slot(index, ARRAY_VALUE)),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
                    Lir::LoadI64(0),
                    Lir::StoreSlot(BIGINT_LOSSLESS_SLOT),
                    Lir::LoadSlotAddress(BIGINT_LOSSLESS_SLOT),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 3 },
                    Lir::CallImport { module: dll.to_string(), field: "napi_get_value_bigint_int64".to_string() },
                    Lir::JumpIfZero(integer_ok),
                    Lir::Jump(FAIL_LABEL),
                    Lir::Label(integer_ok),
                    Lir::LoadSlotAddress(BIGINT_LOSSLESS_SLOT),
                    Lir::LoadByteFromRax,
                    Lir::JumpIfZero(range_failure_label),
                    // `array[index] = value`。
                    Lir::LoadSlotToRcx(array_slot(index, ARRAY_INDEX)),
                    Lir::LoadSlotToRdx(array_slot(index, ARRAY_VALUE)),
                    Lir::LoadSlot(out_slot(index)),
                    Lir::StoreIndexedRdxToRaxRcx,
                    // `index += 1`，条件与入口一致。
                    Lir::LoadSlot(array_slot(index, ARRAY_INDEX)),
                    Lir::AddRaxImmediate(1),
                    Lir::StoreSlot(array_slot(index, ARRAY_INDEX)),
                    Lir::LoadSlot(array_slot(index, ARRAY_INDEX)),
                    Lir::LoadSlotToRcx(array_slot(index, ARRAY_COUNT)),
                    Lir::Lt,
                    Lir::StoreSlot(array_slot(index, ARRAY_CONDITION)),
                    Lir::LoadSlot(array_slot(index, ARRAY_CONDITION)),
                    Lir::JumpIfZero(fill_end),
                    Lir::Jump(fill_loop),
                    Lir::Label(fill_end),
                ]);
            }
            HostParameterKind::Utf8 => {
                // 字符串 → `Utf8Text` 对象：与编译期文本常量同一载体
                // （`{ [0] = _repr }`，字节逐槽 8 字节）。先探长度，再用
                // 连续暂存缓冲接收 UTF-8 字节，最后分散进字节数组。
                let ordinal = text_param_ordinal(index);
                let label_base = 700u32 + u32::from(ordinal) * 10;
                let length_ok = label_base;
                let copy_ok = label_base + 1;
                let copy_loop = label_base + 2;
                let copy_end = label_base + 3;
                let mut block = vec![
                    Lir::LoadI64(0),
                    Lir::StoreSlot(text_param_slot(index, TP_LEN)),
                    Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                    Lir::LoadSlotToArg { slot: argv_slot(index), index: 1 },
                    Lir::LoadSlotToArg { slot: ZERO_SLOT, index: 2 },
                    Lir::LoadSlotToArg { slot: ZERO_SLOT, index: 3 },
                ];
                if platform.is_windows() {
                    block.push(Lir::StoreSlotAddressToStackArg { slot: text_param_slot(index, TP_LEN), index: 0 });
                }
                else {
                    block.push(Lir::LoadSlotAddress(text_param_slot(index, TP_LEN)));
                    block.push(Lir::StoreSlot(TEMP_SLOT));
                    block.push(Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 4 });
                }
                block.extend([
                    Lir::CallImport { module: dll.to_string(), field: "napi_get_value_string_utf8".to_string() },
                    // 非字符串返回 `napi_string_expected`；按类型错误报告。
                    Lir::JumpIfZero(length_ok),
                    Lir::Jump(FAIL_LABEL),
                    Lir::Label(length_ok),
                    // 暂存缓冲：count = 字节数 + 1（尾部留给 NUL），payload 为 8 字节槽。
                    Lir::LoadSlot(text_param_slot(index, TP_LEN)),
                    Lir::AddRaxImmediate(1),
                ]);
                block.extend(array_runtime::array_allocate(
                    allocator,
                    ArrayAllocationSlots {
                        count: text_param_slot(index, TP_SCRATCH_COUNT),
                        bytes: text_param_slot(index, TP_SCRATCH_BYTES),
                        auxiliary: text_param_slot(index, TP_SCRATCH_AUX),
                        condition: text_param_slot(index, TP_SCRATCH_COND),
                    },
                    allocation_failure_label,
                ));
                block.extend([
                    Lir::StoreSlot(text_param_slot(index, TP_SCRATCH)),
                    Lir::LoadI64(1),
                    Lir::StoreSlot(text_param_slot(index, TP_SCRATCH_FLAG)),
                    // 复制：`napi_get_value_string_utf8(env, argv, buf, bufsize, &written)`。
                    Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                    Lir::LoadSlotToArg { slot: argv_slot(index), index: 1 },
                    Lir::LoadSlot(text_param_slot(index, TP_SCRATCH)),
                    Lir::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
                    Lir::LoadSlot(text_param_slot(index, TP_SCRATCH_BYTES)),
                    Lir::SubRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET),
                    Lir::StoreSlot(TEMP_SLOT),
                    Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 3 },
                ]);
                if platform.is_windows() {
                    block.push(Lir::StoreSlotAddressToStackArg { slot: text_param_slot(index, TP_INDEX), index: 0 });
                }
                else {
                    block.push(Lir::LoadSlotAddress(text_param_slot(index, TP_INDEX)));
                    block.push(Lir::StoreSlot(TEMP_SLOT));
                    block.push(Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 4 });
                }
                block.extend([
                    Lir::CallImport { module: dll.to_string(), field: "napi_get_value_string_utf8".to_string() },
                    Lir::JumpIfZero(copy_ok),
                    // 已分配暂存缓冲：走分配失败出口，先释放再报错。
                    Lir::Jump(allocation_failure_label),
                    Lir::Label(copy_ok),
                    // 字节数组：count = 字节数。
                    Lir::LoadSlot(text_param_slot(index, TP_LEN)),
                ]);
                block.extend(array_runtime::array_allocate(
                    allocator,
                    ArrayAllocationSlots {
                        count: text_param_slot(index, TP_ARRAY_COUNT),
                        bytes: text_param_slot(index, TP_ARRAY_BYTES),
                        auxiliary: text_param_slot(index, TP_ARRAY_AUX),
                        condition: text_param_slot(index, TP_ARRAY_COND),
                    },
                    allocation_failure_label,
                ));
                block.extend([
                    Lir::StoreSlot(text_param_slot(index, TP_ARRAY)),
                    Lir::LoadI64(1),
                    Lir::StoreSlot(text_param_slot(index, TP_ARRAY_FLAG)),
                    // 对象块：count = 1，借用元素区 8 字节存放 `_repr` 指针。
                    Lir::LoadI64(1),
                ]);
                block.extend(array_runtime::array_allocate(
                    allocator,
                    ArrayAllocationSlots {
                        count: text_param_slot(index, TP_OBJECT_COUNT),
                        bytes: text_param_slot(index, TP_OBJECT_BYTES),
                        auxiliary: text_param_slot(index, TP_OBJECT_AUX),
                        condition: text_param_slot(index, TP_OBJECT_COND),
                    },
                    allocation_failure_label,
                ));
                block.extend([
                    Lir::StoreSlot(text_param_slot(index, TP_OBJECT)),
                    Lir::LoadI64(1),
                    Lir::StoreSlot(text_param_slot(index, TP_OBJECT_FLAG)),
                    // `object[0] = byte_array`。
                    Lir::LoadSlotToRcx(text_param_slot(index, TP_ARRAY)),
                    Lir::LoadSlot(text_param_slot(index, TP_OBJECT)),
                    Lir::StoreRcxToRaxDisplacement(array_runtime::ARRAY_ELEMENT_OFFSET as i32),
                    // 字节游标与下标。
                    Lir::LoadSlot(text_param_slot(index, TP_SCRATCH)),
                    Lir::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET),
                    Lir::StoreSlot(text_param_slot(index, TP_CURSOR)),
                    Lir::LoadI64(0),
                    Lir::StoreSlot(text_param_slot(index, TP_INDEX)),
                    Lir::Label(copy_loop),
                    Lir::LoadSlot(text_param_slot(index, TP_INDEX)),
                    Lir::LoadSlotToRcx(text_param_slot(index, TP_LEN)),
                    Lir::Lt,
                    Lir::StoreSlot(text_param_slot(index, TP_ARRAY_COND)),
                    Lir::LoadSlot(text_param_slot(index, TP_ARRAY_COND)),
                    Lir::JumpIfZero(copy_end),
                    Lir::LoadSlot(text_param_slot(index, TP_CURSOR)),
                    Lir::LoadByteFromRax,
                    Lir::StoreSlot(text_param_slot(index, TP_ELEMENT)),
                    Lir::LoadSlot(text_param_slot(index, TP_ARRAY)),
                    Lir::LoadSlotToRdx(text_param_slot(index, TP_ELEMENT)),
                    Lir::LoadSlotToRcx(text_param_slot(index, TP_INDEX)),
                    Lir::StoreIndexedRdxToRaxRcx,
                    Lir::LoadSlot(text_param_slot(index, TP_CURSOR)),
                    Lir::AddRaxImmediate(1),
                    Lir::StoreSlot(text_param_slot(index, TP_CURSOR)),
                    Lir::LoadSlot(text_param_slot(index, TP_INDEX)),
                    Lir::AddRaxImmediate(1),
                    Lir::StoreSlot(text_param_slot(index, TP_INDEX)),
                    Lir::Jump(copy_loop),
                    Lir::Label(copy_end),
                ]);
                instructions.extend(block);
            }
        }
    }
    for (index, kind) in method.parameters.iter().enumerate() {
        let slot = match kind {
            HostParameterKind::Int64 | HostParameterKind::Int64Array => out_slot(index),
            // 文本实参传对象块地址（`_repr` 字段在对象首地址）。
            HostParameterKind::Utf8 => {
                instructions.push(Lir::LoadSlot(text_param_slot(index, TP_OBJECT)));
                instructions.push(Lir::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET));
                instructions.push(Lir::StoreSlot(TEMP_SLOT));
                TEMP_SLOT
            }
        };
        instructions.push(Lir::LoadSlotToArg { slot, index: index as u8 });
    }
    instructions.push(Lir::Call(method.symbol.clone()));
    // 返回值先落槽：数组写回会借用 `rax` 作循环载体，不能把返回寄存器留到最后。
    if !matches!(method.returns, HostValueKind::Unit) {
        instructions.push(Lir::StoreSlot(RESULT_SLOT));
    }
    // 数组实参的原地修改对宿主可见：调用后逐元素写回。
    for (index, kind) in method.parameters.iter().enumerate() {
        if !matches!(kind, HostParameterKind::Int64Array) {
            continue;
        }
        let label_base = 100 + index as u32 * 10;
        let writeback_loop = label_base + 5;
        let writeback_end = label_base + 6;
        instructions.extend([
            Lir::LoadI64(0),
            Lir::StoreSlot(array_slot(index, ARRAY_INDEX)),
            Lir::LoadSlot(array_slot(index, ARRAY_INDEX)),
            Lir::LoadSlotToRcx(array_slot(index, ARRAY_COUNT)),
            Lir::Lt,
            Lir::StoreSlot(array_slot(index, ARRAY_CONDITION)),
            Lir::LoadSlot(array_slot(index, ARRAY_CONDITION)),
            Lir::JumpIfZero(writeback_end),
            Lir::Label(writeback_loop),
            // `value = array[index]`。
            Lir::LoadSlotToRcx(array_slot(index, ARRAY_INDEX)),
            Lir::LoadSlot(out_slot(index)),
            Lir::LoadIndexedFromRaxRcx,
            Lir::StoreSlot(array_slot(index, ARRAY_VALUE)),
            // `napi_create_bigint_int64(env, value, &element)`。
            Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
            Lir::LoadSlotToArg { slot: array_slot(index, ARRAY_VALUE), index: 1 },
            Lir::LoadSlotAddress(array_slot(index, ARRAY_ELEMENT)),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
            Lir::CallImport { module: dll.to_string(), field: "napi_create_bigint_int64".to_string() },
            // `napi_set_element(env, array, index, element)`。
            Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
            Lir::LoadSlotToArg { slot: argv_slot(index), index: 1 },
            Lir::LoadSlotToArg { slot: array_slot(index, ARRAY_INDEX), index: 2 },
            Lir::LoadSlotToArg { slot: array_slot(index, ARRAY_ELEMENT), index: 3 },
            Lir::CallImport { module: dll.to_string(), field: "napi_set_element".to_string() },
            Lir::LoadSlot(array_slot(index, ARRAY_INDEX)),
            Lir::AddRaxImmediate(1),
            Lir::StoreSlot(array_slot(index, ARRAY_INDEX)),
            Lir::LoadSlot(array_slot(index, ARRAY_INDEX)),
            Lir::LoadSlotToRcx(array_slot(index, ARRAY_COUNT)),
            Lir::Lt,
            Lir::StoreSlot(array_slot(index, ARRAY_CONDITION)),
            Lir::LoadSlot(array_slot(index, ARRAY_CONDITION)),
            Lir::JumpIfZero(writeback_end),
            Lir::Jump(writeback_loop),
            Lir::Label(writeback_end),
        ]);
    }
    // 返回值先编组到宿主值，再释放镜像副本：guest 可以原样返回某个实参镜像
    // （文本或数组），复制必须先于释放。各分支只写结果槽，不提前返回。
    match method.returns {
        HostValueKind::Utf8 => {
            // 构造失败时保持 `undefined`，不读未初始化的帧槽。
            instructions.extend([Lir::LoadI64(0), Lir::StoreSlot(STRING_SLOT)]);
            // guest 文本值是 `Utf8Text` 对象：`{ [0] = _repr 数组 }`，数组逐字节占
            // 一个 8 字节槽（数组运行时合同）。桥把字节 gather 进一段连续暂存缓冲
            // （同一分配/释放实现），`napi_create_string_utf8` 复制后立即释放——
            // 暂存缓冲的有效期就是这一次调用。
            instructions.extend([
                Lir::LoadSlot(RESULT_SLOT),
                Lir::LoadFromRaxDisplacement(0),
                Lir::StoreSlot(text_slot(0)),
                Lir::LoadSlot(text_slot(0)),
                Lir::LoadFromRaxDisplacement(0),
                Lir::StoreSlot(text_slot(1)),
            ]);
            // 暂存缓冲：`array_allocate(count)`（头部之外按 8 字节槽留足字节）。
            instructions.push(Lir::LoadSlot(text_slot(1)));
            instructions.extend(array_runtime::array_allocate(
                allocator,
                ArrayAllocationSlots { count: text_slot(1), bytes: text_slot(2), auxiliary: text_slot(3), condition: text_slot(4) },
                allocation_failure_label,
            ));
            instructions.extend([
                Lir::StoreSlot(text_slot(5)),
                Lir::LoadI64(0),
                Lir::StoreSlot(text_slot(6)),
                Lir::LoadSlot(text_slot(5)),
                Lir::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET),
                Lir::StoreSlot(text_slot(7)),
                Lir::Label(GATHER_LOOP_LABEL),
                // `index < count`。
                Lir::LoadSlot(text_slot(6)),
                Lir::LoadSlotToRcx(text_slot(1)),
                Lir::Lt,
                Lir::StoreSlot(text_slot(4)),
                Lir::LoadSlot(text_slot(4)),
                Lir::JumpIfZero(GATHER_END_LABEL),
                // `*cursor = array[index]`（低字节）。
                Lir::LoadSlot(text_slot(0)),
                Lir::LoadSlotToRcx(text_slot(6)),
                Lir::LoadIndexedFromRaxRcx,
                Lir::StoreSlot(text_slot(8)),
                Lir::LoadSlot(text_slot(7)),
                Lir::LoadSlotToRdx(text_slot(8)),
                Lir::StoreByteAtRaxFromRdx,
                Lir::LoadSlot(text_slot(7)),
                Lir::AddRaxImmediate(1),
                Lir::StoreSlot(text_slot(7)),
                Lir::LoadSlot(text_slot(6)),
                Lir::AddRaxImmediate(1),
                Lir::StoreSlot(text_slot(6)),
                Lir::Jump(GATHER_LOOP_LABEL),
                Lir::Label(GATHER_END_LABEL),
                // `napi_create_string_utf8(env, bytes, count, &out)`。
                Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                Lir::LoadSlot(text_slot(5)),
                Lir::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET),
                Lir::StoreSlot(text_slot(8)),
                Lir::LoadSlotToArg { slot: text_slot(8), index: 1 },
                Lir::LoadSlotToArg { slot: text_slot(1), index: 2 },
                Lir::LoadSlotAddress(STRING_SLOT),
                Lir::StoreSlot(TEMP_SLOT),
                Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 3 },
                Lir::CallImport { module: dll.to_string(), field: "napi_create_string_utf8".to_string() },
                // 释放暂存缓冲；创建失败也走同一释放（`out` 保持未写入，返回 `NULL`）。
                Lir::LoadSlot(text_slot(5)),
            ]);
            instructions.extend(array_runtime::array_free(
                allocator,
                ArrayAllocationSlots { count: text_slot(1), bytes: text_slot(2), auxiliary: text_slot(3), condition: text_slot(4) },
                ZERO_SLOT,
            ));
        }
        HostValueKind::Int64Array => {
            // guest 返回 `[i64]` 堆数组：`[0]` 元素个数，元素从 `+16` 起按 8 字节槽。
            // 逐元素复制为 JS `bigint` 序列；guest 数组归语言值图，桥不释放。
            instructions.extend([Lir::LoadI64(0), Lir::StoreSlot(array_return_slot(AR_JS_ARRAY))]);
            instructions.extend([
                Lir::LoadSlot(RESULT_SLOT),
                Lir::LoadFromRaxDisplacement(0),
                Lir::StoreSlot(array_return_slot(AR_COUNT)),
                // 个数必须落在 `[0, ARRAY_MAX_ELEMENTS]`；否则宿主值无法物化。
                Lir::LoadI64(0),
                Lir::StoreSlot(array_return_slot(AR_CONDITION)),
                Lir::LoadSlot(array_return_slot(AR_COUNT)),
                Lir::LoadSlotToRcx(array_return_slot(AR_CONDITION)),
                Lir::Ge,
                Lir::StoreSlot(array_return_slot(AR_CONDITION)),
                Lir::LoadSlot(array_return_slot(AR_CONDITION)),
                Lir::JumpIfZero(allocation_failure_label),
                Lir::LoadI64(array_runtime::ARRAY_MAX_ELEMENTS),
                Lir::StoreSlot(array_return_slot(AR_CONDITION)),
                Lir::LoadSlot(array_return_slot(AR_COUNT)),
                Lir::LoadSlotToRcx(array_return_slot(AR_CONDITION)),
                Lir::Le,
                Lir::StoreSlot(array_return_slot(AR_CONDITION)),
                Lir::LoadSlot(array_return_slot(AR_CONDITION)),
                Lir::JumpIfZero(allocation_failure_label),
                // `napi_create_array_with_length(env, count, &array)`。
                Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                Lir::LoadSlotToArg { slot: array_return_slot(AR_COUNT), index: 1 },
                Lir::LoadSlotAddress(array_return_slot(AR_JS_ARRAY)),
                Lir::StoreSlot(TEMP_SLOT),
                Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
                Lir::CallImport { module: dll.to_string(), field: "napi_create_array_with_length".to_string() },
                Lir::LoadI64(0),
                Lir::StoreSlot(array_return_slot(AR_INDEX)),
                Lir::Label(900),
                Lir::LoadSlot(array_return_slot(AR_INDEX)),
                Lir::LoadSlotToRcx(array_return_slot(AR_COUNT)),
                Lir::Lt,
                Lir::StoreSlot(array_return_slot(AR_CONDITION)),
                Lir::LoadSlot(array_return_slot(AR_CONDITION)),
                Lir::JumpIfZero(901),
                // `value = result[index]`。
                Lir::LoadSlot(RESULT_SLOT),
                Lir::LoadSlotToRcx(array_return_slot(AR_INDEX)),
                Lir::LoadIndexedFromRaxRcx,
                Lir::StoreSlot(array_return_slot(AR_VALUE)),
                // `napi_create_bigint_int64(env, value, &element)`。
                Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                Lir::LoadSlotToArg { slot: array_return_slot(AR_VALUE), index: 1 },
                Lir::LoadSlotAddress(array_return_slot(AR_ELEMENT)),
                Lir::StoreSlot(TEMP_SLOT),
                Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
                Lir::CallImport { module: dll.to_string(), field: "napi_create_bigint_int64".to_string() },
                // `napi_set_element(env, array, index, element)`。
                Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                Lir::LoadSlotToArg { slot: array_return_slot(AR_JS_ARRAY), index: 1 },
                Lir::LoadSlotToArg { slot: array_return_slot(AR_INDEX), index: 2 },
                Lir::LoadSlotToArg { slot: array_return_slot(AR_ELEMENT), index: 3 },
                Lir::CallImport { module: dll.to_string(), field: "napi_set_element".to_string() },
                Lir::LoadSlot(array_return_slot(AR_INDEX)),
                Lir::AddRaxImmediate(1),
                Lir::StoreSlot(array_return_slot(AR_INDEX)),
                Lir::Jump(900),
                Lir::Label(901),
            ]);
        }
        HostValueKind::Bool | HostValueKind::Int64 => {
            let field = match method.returns {
                HostValueKind::Bool => "napi_get_boolean",
                HostValueKind::Int64 => "napi_create_bigint_int64",
                HostValueKind::Utf8 | HostValueKind::Int64Array | HostValueKind::Unit => unreachable!("matched above"),
            };
            instructions.extend([
                // `napi_create_bigint_int64(env, value, &result)` / `napi_get_boolean(env, value, &result)`。
                Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
                Lir::LoadSlotToArg { slot: RESULT_SLOT, index: 1 },
                Lir::LoadSlotAddress(RESULT_SLOT),
                Lir::StoreSlot(TEMP_SLOT),
                Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
                Lir::CallImport { module: dll.to_string(), field: field.to_string() },
            ]);
        }
        HostValueKind::Unit => {
            // 回调返回 `NULL`：Node-API 把 `undefined` 交给调用方，不伪造值。
            instructions.extend([Lir::LoadI64(0), Lir::StoreSlot(RESULT_SLOT)]);
        }
    }
    // 释放镜像侧数组副本与文本临时副本：生命周期就是本次宿主调用。
    // guest 数组值图的分配不在这里回收（native lane 没有逃逸分析或 GC）。
    for (index, kind) in method.parameters.iter().enumerate() {
        if !matches!(kind, HostParameterKind::Int64Array) {
            continue;
        }
        instructions.push(Lir::LoadI64(0));
        instructions.push(Lir::StoreSlot(array_slot(index, ARRAY_VALUE)));
        instructions.push(Lir::LoadSlot(out_slot(index)));
        instructions.extend(array_runtime::array_free(
            allocator,
            ArrayAllocationSlots {
                count: array_slot(index, ARRAY_COUNT),
                bytes: array_slot(index, ARRAY_VALUE),
                auxiliary: array_slot(index, ARRAY_HEAP),
                condition: array_slot(index, ARRAY_CONDITION),
            },
            array_slot(index, ARRAY_VALUE),
        ));
    }
    emit_free_text_temporaries(&mut instructions, 800);
    instructions.extend([
        Lir::LoadSlot(match method.returns {
            HostValueKind::Utf8 => STRING_SLOT,
            HostValueKind::Int64Array => array_return_slot(AR_JS_ARRAY),
            HostValueKind::Bool | HostValueKind::Int64 | HostValueKind::Unit => RESULT_SLOT,
        }),
        Lir::Return,
    ]);
    // 类型错误出口：先释放本次调用已分配的镜像副本，再 `napi_throw_type_error`。
    instructions.push(Lir::Label(FAIL_LABEL));
    emit_free_allocated(&mut instructions, 300);
    emit_free_text_temporaries(&mut instructions, 810);
    instructions.extend([
        Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
        Lir::LoadSlotToArg { slot: ZERO_SLOT, index: 1 },
        Lir::LoadDataAddress(error_message_offset),
        Lir::StoreSlot(TEMP_SLOT),
        Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
        Lir::CallImport { module: dll.to_string(), field: "napi_throw_type_error".to_string() },
        Lir::LoadI64(0),
        Lir::Return,
    ]);
    // `bigint` 超出 `i64` 域：释放副本后抛 `RangeError`。
    instructions.push(Lir::Label(range_failure_label));
    emit_free_allocated(&mut instructions, 400);
    emit_free_text_temporaries(&mut instructions, 820);
    instructions.extend([
        Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
        Lir::LoadSlotToArg { slot: ZERO_SLOT, index: 1 },
        Lir::LoadDataAddress(range_error_offset),
        Lir::StoreSlot(TEMP_SLOT),
        Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
        Lir::CallImport { module: dll.to_string(), field: "napi_throw_range_error".to_string() },
        Lir::LoadI64(0),
        Lir::Return,
    ]);
    if array_count > 0 || text_param_count > 0 || returns_array || returns_text {
        // 宿主载体分配失败（容量溢出或分配实现报错）走宿主 error，而不是 Type 错误；
        // 仍先释放此前已分配成功的镜像副本。
        instructions.push(Lir::Label(allocation_failure_label));
        emit_free_allocated(&mut instructions, 500);
        emit_free_text_temporaries(&mut instructions, 830);
        instructions.extend([
            Lir::LoadSlotToArg { slot: ENV_SLOT, index: 0 },
            Lir::LoadSlotToArg { slot: ZERO_SLOT, index: 1 },
            Lir::LoadDataAddress(allocation_error_offset),
            Lir::StoreSlot(TEMP_SLOT),
            Lir::LoadSlotToArg { slot: TEMP_SLOT, index: 2 },
            Lir::CallImport { module: dll.to_string(), field: "napi_throw_error".to_string() },
            Lir::LoadI64(0),
            Lir::Return,
        ]);
    }

    X64ProgramFunction { name: node_wrapper_name(&method.symbol), slot_count, instructions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lowering::backends::machine::{X64CallConv, x64_program::encode_x64_program};

    fn array_method() -> HostMethod {
        HostMethod {
            name: "mutate".to_string(),
            symbol: "mutate".to_string(),
            parameters: vec![HostParameterKind::Int64Array],
            returns: HostValueKind::Int64,
        }
    }

    fn import_fields(wrapper: &X64ProgramFunction) -> Vec<&str> {
        wrapper
            .instructions
            .iter()
            .filter_map(|instruction| match instruction {
                X64TextInstruction::CallImport { field, .. } => Some(field.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Node-API 的 `i64` 只走 bigint：不得再出现截断的 `napi_get_value_int64`。
    #[test]
    fn node_wrapper_marshals_i64_as_bigint_only() {
        let wrapper = node_method_wrapper(&array_method(), 0, 1, 2, X64Platform::windows_x64(), "node.exe", None);
        let fields = import_fields(&wrapper);
        assert!(!fields.contains(&"napi_get_value_int64"), "i64 入参不得回落到 number 载体");
        assert!(fields.contains(&"napi_get_value_bigint_int64"), "i64 入参必须走 bigint + lossless");
        assert!(fields.contains(&"napi_create_bigint_int64"), "i64 返回值与数组写回必须走 bigint");
        assert!(fields.contains(&"napi_throw_range_error"), "lossless 失败必须是 RangeError");
    }

    /// 标量位于数组之前时，数组槽仍必须落在帧内（槽位按数组序号而非形参下标）。
    #[test]
    fn array_slots_stay_within_frame_when_scalar_precedes_array() {
        let method = HostMethod {
            name: "mixed".to_string(),
            symbol: "mixed".to_string(),
            parameters: vec![HostParameterKind::Int64, HostParameterKind::Int64Array],
            returns: HostValueKind::Int64,
        };
        let wrapper = node_method_wrapper(&method, 0, 1, 2, X64Platform::windows_x64(), "node.exe", None);
        let debug = format!("{:?}", wrapper.instructions);
        let mut max_slot = 0u16;
        for needle in ["slot: ", "LoadSlot(", "StoreSlot(", "LoadSlotAddress(", "LoadSlotToRcx("] {
            let mut rest = debug.as_str();
            while let Some(position) = rest.find(needle) {
                rest = &rest[position + needle.len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(slot) = digits.parse::<u16>() {
                    max_slot = max_slot.max(slot);
                }
            }
        }
        assert!(
            max_slot < wrapper.slot_count,
            "最大引用槽 {max_slot} 必须落在帧大小 {} 之内",
            wrapper.slot_count
        );
    }

    /// 部分编组失败：类型错误出口必须先释放已分配的镜像副本，再抛宿主错误。
    #[test]
    fn type_failure_frees_allocated_array_mirrors_before_throwing() {
        let wrapper = node_method_wrapper(&array_method(), 0, 1, 2, X64Platform::windows_x64(), "node.exe", None);
        let fail_label = wrapper
            .instructions
            .iter()
            .position(|instruction| matches!(instruction, X64TextInstruction::Label(0)))
            .expect("type failure label");
        let throw = wrapper
            .instructions
            .iter()
            .position(|instruction| {
                matches!(
                    instruction,
                    X64TextInstruction::CallImport { field, .. } if field == "napi_throw_type_error"
                )
            })
            .expect("type error throw");
        assert!(fail_label < throw);
        assert!(
            wrapper.instructions[fail_label..throw].iter().any(|instruction| {
                matches!(
                    instruction,
                    X64TextInstruction::CallImport { field, .. } if field == "HeapFree"
                )
            }),
            "失败清理必须在抛错前释放镜像副本"
        );
    }

    /// 字符串返回使用 UTF-8 载体，不经过 UTF-16 码元。
    #[test]
    fn string_return_uses_utf8_bytes() {
        let method = HostMethod { name: "text".to_string(), symbol: "text".to_string(), parameters: Vec::new(), returns: HostValueKind::Utf8 };
        let wrapper = node_method_wrapper(&method, 0, 1, 2, X64Platform::windows_x64(), "node.exe", None);
        let fields = import_fields(&wrapper);
        assert!(fields.contains(&"napi_create_string_utf8"));
        assert!(!fields.contains(&"napi_create_string_utf16"), "字符串长度必须是 UTF-8 字节而不是 UTF-16 码元");
    }

    fn max_referenced_slot(program: &X64ProgramFunction) -> u16 {
        let debug = format!("{:?}", program.instructions);
        let mut max_slot = 0u16;
        for needle in ["slot: ", "LoadSlot(", "StoreSlot(", "LoadSlotAddress(", "LoadSlotToRcx("] {
            let mut rest = debug.as_str();
            while let Some(position) = rest.find(needle) {
                rest = &rest[position + needle.len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(slot) = digits.parse::<u16>() {
                    max_slot = max_slot.max(slot);
                }
            }
        }
        max_slot
    }

    /// `utf8` 实参：按 UTF-8 字节复制到 `Utf8Text` 对象，三段桥侧临时副本都要释放。
    #[test]
    fn utf8_parameter_copies_bytes_and_releases_temporaries() {
        let method =
            HostMethod { name: "echo".to_string(), symbol: "echo".to_string(), parameters: vec![HostParameterKind::Utf8], returns: HostValueKind::Utf8 };
        let wrapper = node_method_wrapper(&method, 0, 1, 2, X64Platform::windows_x64(), "node.exe", None);
        let fields = import_fields(&wrapper);
        assert!(fields.contains(&"napi_get_value_string_utf8"), "文本实参必须走 UTF-8 字节查询与复制");
        assert!(fields.contains(&"napi_create_string_utf8"), "文本返回必须走 UTF-8 字节");
        let frees = wrapper
            .instructions
            .iter()
            .filter(|instruction| matches!(instruction, X64TextInstruction::CallImport { field, .. } if field == "HeapFree"))
            .count();
        assert!(frees >= 3, "暂存缓冲、字节数组、对象块都必须释放（实际 {frees} 次）");
        assert!(max_referenced_slot(&wrapper) < wrapper.slot_count, "文本编组槽必须落在帧内");
    }

    /// 只有文本返回、没有任何实参编组时，暂存缓冲的分配失败出口也必须定义。
    #[test]
    fn text_return_only_defines_every_jump_label() {
        let method =
            HostMethod { name: "greeting".to_string(), symbol: "greeting".to_string(), parameters: vec![], returns: HostValueKind::Utf8 };
        let wrapper = node_method_wrapper(&method, 0, 1, 2, X64Platform::windows_x64(), "node.exe", None);
        let mut defined = std::collections::BTreeSet::new();
        let mut referenced = Vec::new();
        for instruction in &wrapper.instructions {
            match instruction {
                X64TextInstruction::Label(label) => {
                    defined.insert(*label);
                }
                X64TextInstruction::Jump(label) | X64TextInstruction::JumpIfZero(label) => referenced.push(*label),
                _ => {}
            }
        }
        for label in referenced {
            assert!(defined.contains(&label), "跳转目标 {label} 未定义");
        }
    }

    /// `[i64]` 返回：先把 guest 数组复制成宿主序列，再释放实参镜像副本。
    #[test]
    fn int64_array_return_is_copied_before_mirror_release() {
        let method = HostMethod {
            name: "round_trip".to_string(),
            symbol: "round_trip".to_string(),
            parameters: vec![HostParameterKind::Int64Array],
            returns: HostValueKind::Int64Array,
        };
        let wrapper = node_method_wrapper(&method, 0, 1, 2, X64Platform::windows_x64(), "node.exe", None);
        let fields = import_fields(&wrapper);
        assert!(fields.contains(&"napi_create_array_with_length"));
        assert!(fields.contains(&"napi_set_element"));
        let marshal = wrapper
            .instructions
            .iter()
            .position(|instruction| {
                matches!(instruction, X64TextInstruction::CallImport { field, .. } if field == "napi_create_array_with_length")
            })
            .expect("array return marshalling");
        let release = wrapper
            .instructions
            .iter()
            .position(|instruction| matches!(instruction, X64TextInstruction::CallImport { field, .. } if field == "HeapFree"))
            .expect("mirror release");
        assert!(marshal < release, "返回数组可能与实参镜像别名：复制必须先于释放");
        assert!(max_referenced_slot(&wrapper) < wrapper.slot_count);
    }

    /// CPython 的 `utf8` 实参按 `O` 接收对象，再显式取 UTF-8 字节指针与长度；
    /// `#` 系格式要求 `PY_SSIZE_T_CLEAN`，本 lane 不链接 Python.h，不能使用。
    #[test]
    fn python_utf8_parameter_uses_object_conversion() {
        let method =
            HostMethod { name: "echo".to_string(), symbol: "echo".to_string(), parameters: vec![HostParameterKind::Utf8], returns: HostValueKind::Utf8 };
        let (data, _, _, format_offsets) = python_module_data("probe", &[], &[method]);
        let start = format_offsets[0] as usize;
        let end = data[start..].iter().position(|byte| *byte == 0).map(|offset| start + offset).expect("format string");
        assert_eq!(std::str::from_utf8(&data[start..end]).expect("format text"), "O:echo");
    }

    /// 静态字符串描述符的长度字段按 UTF-8 字节数编码（`é中` = 5 字节，不是 2 个码元）。
    #[test]
    fn static_string_descriptor_length_is_utf8_byte_count() {
        let value = "é中";
        let program = X64ProgramFunction {
            name: "main".to_string(),
            slot_count: 1,
            instructions: vec![X64TextInstruction::LoadString(value.to_string()), X64TextInstruction::Return],
        };
        let image = encode_x64_program(&[program], X64CallConv::Win64).expect("encode static string probe");
        let bytes = value.as_bytes();
        assert_eq!(bytes.len(), 5);
        let position = image.text.windows(bytes.len()).position(|window| window == bytes).expect("utf-8 bytes in .text");
        assert_eq!(
            &image.text[position - 4..position],
            &(bytes.len() as u32).to_le_bytes(),
            "描述符长度必须是 UTF-8 字节数"
        );
    }
}
