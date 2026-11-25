//! `[T]` 的数组运行时合同（target-lane 私有，唯一生产路径）。
//!
//! 本模块冻结 x86-64 堆数组的**物理表示、分配与释放实现、边界检查位置与失败出口**。
//! guest lowering（[`super::x86_64`]）与 Node 桥（[`super::carrier`]）都只消费这里的
//! 发射序列，不再各自计算字节数、写数组头或直接调用 `HeapAlloc`。
//!
//! 布局（元素槽固定 8 字节，元素类型必须是有 8 字节槽载体的类型）：
//!
//! ```text
//! [0]   i64 元素个数（语言可见长度，也是边界检查的唯一上界）
//! [8]   i64 分配字节数（释放实现用：Linux `munmap` 需要原长度）
//! [16]  元素 0（8 字节槽、8 字节对齐）
//! [16 + 8*i]  元素 i
//! ```
//!
//! 元素载体由 [`super::x86_64`] 的槽载体合同裁决（`is_local_carrier`），不在本模块
//! 另立类型白名单：整数/布尔/字符按各自位宽落在 8 字节槽，浮点按位模式落槽（`f32`
//! 只保证低 32 位有效），文本/数组/元组/sum/引用聚合落 8 字节指针。分配实现整体
//! 零初始化，于是默认值就是各自类型的零表示（`0`、`0.0`、空引用）。
//!
//! `ArrayStorage` 的「初始化前沿」仍归 Valkyrie 标准库，未初始化槽不因零值而变成
//! 有效元素。定长值聚合 `[T; N]` 不走本表示。
//!
//! 失败语义（三条都走显式分支，不用空指针的后续故障充当错误通道）：
//!
//! - 元素个数为负或 `8 * n + 16` 溢出：无符号范围检查失败；
//! - 分配实现返回 `NULL` / 负 errno：分配失败；
//! - `array.get` / `array.set` 的下标不在 `[0, [0])`：边界检查失败。
//!
//! 释放责任：guest 里的 `[T]` 归语言值图，native lane 没有回收通道，后端**不**
//! 释放；宿主桥为一次调用建立的临时副本在写回后由桥释放，其有效期就是该次调用。

use super::{X64Platform, X64TextInstruction as Lir, x86_64::X86_64OperatorCode};

/// `[T]` 头部字节数：元素个数 + 分配字节数。
pub const ARRAY_HEADER_BYTES: i64 = 16;
/// 元素个数在头部内的偏移。
pub const ARRAY_COUNT_OFFSET: i32 = 0;
/// 分配字节数在头部内的偏移（释放实现读回原长度）。
pub const ARRAY_ALLOCATION_BYTES_OFFSET: i32 = 8;
/// 元素槽字节宽度（元素载体必须落进单个 8 字节槽）。
pub const ARRAY_ELEMENT_BYTES: i64 = 8;
/// 首个元素相对数组指针的偏移。
pub const ARRAY_ELEMENT_OFFSET: i32 = 16;
/// 元素槽对齐（分配实现必须满足；Win64 堆与 Linux `mmap` 都按页对齐）。
pub const ARRAY_ELEMENT_ALIGNMENT: i64 = 8;

/// 单次分配允许的最大元素个数：保证 `16 + 8 * n` 不溢出 `i64`。
pub const ARRAY_MAX_ELEMENTS: i64 = (i64::MAX - ARRAY_HEADER_BYTES) / ARRAY_ELEMENT_BYTES;

/// unite/sum 载体字节数：`{u32 tag, 4 字节填充, i64 payload}`。
pub const SUM_CARRIER_BYTES: i64 = 16;
/// sum 载荷相对载体的偏移。
pub const SUM_PAYLOAD_OFFSET: i32 = 8;

// 布局在编译期冻结：头两格、首元素偏移与对齐必须互相一致，改动要么整体更新，
// 要么在构建时失败（不允许某条路径悄悄按旧偏移发射）。
const _: () = {
    assert!(ARRAY_COUNT_OFFSET == 0);
    assert!(ARRAY_ALLOCATION_BYTES_OFFSET == 8);
    assert!(ARRAY_ELEMENT_OFFSET as i64 == ARRAY_HEADER_BYTES);
    assert!(ARRAY_ELEMENT_ALIGNMENT == ARRAY_ELEMENT_BYTES);
};

/// `GetProcessHeap`（Win64 默认堆句柄）。
const GET_PROCESS_HEAP: &str = "GetProcessHeap";
/// `HeapAlloc`。
const HEAP_ALLOC: &str = "HeapAlloc";
/// `HeapFree`。
const HEAP_FREE: &str = "HeapFree";
/// `HeapAlloc` 的 `HEAP_ZERO_MEMORY`（分配即零初始化）。
const HEAP_ZERO_MEMORY: i64 = 0x0000_0008;
/// Win64 分配与释放共用的宿主模块名。
const KERNEL32_DLL: &str = "kernel32.dll";

/// 由完整目标选择的数组分配/释放实现。
///
/// 选择来自已解析的平台身份（容器 + ABI），不由调用点按 OS 猜：
/// `PE32+` 走进程堆，`ELF64` 走 `mmap`/`munmap` 系统调用（不依赖宿主 libc，
/// 静态 `ET_EXEC` 与 `ET_DYN` 扩展都可用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayAllocator {
    /// Win64：`GetProcessHeap` + `HeapAlloc(HEAP_ZERO_MEMORY)` / `HeapFree`。
    WindowsProcessHeap,
    /// SysV64/Linux：`mmap`（匿名、私有、读写）与 `munmap` 系统调用。
    LinuxMmap,
}

impl ArrayAllocator {
    /// 按冻结平台身份选择实现。
    pub fn for_platform(platform: X64Platform) -> Self {
        if platform.is_windows() { Self::WindowsProcessHeap } else { Self::LinuxMmap }
    }
}

/// 一次数组分配需要的帧槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArrayAllocationSlots {
    /// 元素个数（输入时由 `rax` 写入）。
    pub count: u16,
    /// 分配字节数（也用于写回头部第二格）。
    pub bytes: u16,
    /// 分配实现句柄/中间地址。
    pub auxiliary: u16,
    /// 条件与临时值。
    pub condition: u16,
}

/// 分配 `count` 个元素槽；输入 `count` 在 `rax`，输出数组指针在 `rax`。
///
/// 所有失败路径都跳到 `failure_label`；调用方必须在该标签处放置定义好的失败出口
/// （guest 是 `Trap`，宿主桥是 `napi_throw_error`）。本序列内联写头部两格。
pub fn array_allocate(
    allocator: ArrayAllocator,
    slots: ArrayAllocationSlots,
    failure_label: u32,
) -> Vec<Lir> {
    let mut out = Vec::new();
    // 保存元素个数。
    out.push(Lir::StoreSlot(slots.count));
    // 无符号范围检查：`MAX < count`（负数的无符号值极大，一并被拒绝）。
    out.push(Lir::LoadI64(ARRAY_MAX_ELEMENTS));
    out.push(Lir::StoreSlot(slots.condition));
    out.push(Lir::LoadSlotToRcx(slots.condition));
    out.push(Lir::LoadSlot(slots.count));
    out.push(Lir::Operator(X86_64OperatorCode::I64CompareLessThanUnsigned));
    out.push(Lir::JumpIfZero(failure_label));
    // 字节数：`16 + 8 * count`（范围检查已保证不溢出）。
    out.push(Lir::LoadSlot(slots.count));
    out.push(Lir::ShiftLeftRaxImmediate(3));
    out.push(Lir::AddRaxImmediate(ARRAY_HEADER_BYTES as i32));
    out.push(Lir::StoreSlot(slots.bytes));
    match allocator {
        ArrayAllocator::WindowsProcessHeap => {
            out.push(Lir::CallImport { module: KERNEL32_DLL.to_string(), field: GET_PROCESS_HEAP.to_string() });
            out.push(Lir::StoreSlot(slots.auxiliary));
            out.push(Lir::LoadSlotToArg { slot: slots.auxiliary, index: 0 });
            out.push(Lir::LoadSlotToArg { slot: slots.bytes, index: 2 });
            out.push(Lir::LoadI64(HEAP_ZERO_MEMORY));
            out.push(Lir::StoreSlot(slots.auxiliary));
            out.push(Lir::LoadSlotToArg { slot: slots.auxiliary, index: 1 });
            out.push(Lir::CallImport { module: KERNEL32_DLL.to_string(), field: HEAP_ALLOC.to_string() });
        }
        ArrayAllocator::LinuxMmap => {
            // `mmap(NULL, bytes, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)`。
            out.push(Lir::LoadSlot(slots.bytes));
            out.push(Lir::SyscallMmap);
        }
    }
    // 分配失败：`NULL` 或负 errno；`addr > 0` 才是成功。
    out.push(Lir::StoreSlot(slots.auxiliary));
    out.push(Lir::LoadI64(0));
    out.push(Lir::StoreSlot(slots.condition));
    out.push(Lir::LoadSlot(slots.auxiliary));
    out.push(Lir::LoadSlotToRcx(slots.condition));
    out.push(Lir::Operator(X86_64OperatorCode::I64CompareGreaterThan));
    out.push(Lir::JumpIfZero(failure_label));
    // 头两格：元素个数与分配字节数。
    out.push(Lir::LoadSlot(slots.auxiliary));
    out.push(Lir::LoadSlotToRcx(slots.count));
    out.push(Lir::StoreRcxToRax);
    out.push(Lir::LoadSlot(slots.auxiliary));
    out.push(Lir::LoadSlotToRcx(slots.bytes));
    out.push(Lir::StoreRcxToRaxDisplacement(ARRAY_ALLOCATION_BYTES_OFFSET));
    // 数组指针作为结果。
    out.push(Lir::LoadSlot(slots.auxiliary));
    out
}

/// 分配一个零初始化的引用对象（`class` / sum 载体）；输入字节数在 `rax`，输出指针在 `rax`。
///
/// 与堆数组共用同一平台分配实现与失败出口，但**没有数组头**：字段偏移由物理布局
/// 直接给出。对象归语言值图，native lane 不提供回收通道。
pub fn object_allocate(
    allocator: ArrayAllocator,
    size_slot: u16,
    auxiliary: u16,
    condition: u16,
    failure_label: u32,
) -> Vec<Lir> {
    let mut out = vec![Lir::StoreSlot(size_slot)];
    match allocator {
        ArrayAllocator::WindowsProcessHeap => {
            out.push(Lir::CallImport { module: KERNEL32_DLL.to_string(), field: GET_PROCESS_HEAP.to_string() });
            out.push(Lir::StoreSlot(auxiliary));
            out.push(Lir::LoadSlotToArg { slot: auxiliary, index: 0 });
            out.push(Lir::LoadSlotToArg { slot: size_slot, index: 2 });
            out.push(Lir::LoadI64(HEAP_ZERO_MEMORY));
            out.push(Lir::StoreSlot(auxiliary));
            out.push(Lir::LoadSlotToArg { slot: auxiliary, index: 1 });
            out.push(Lir::CallImport { module: KERNEL32_DLL.to_string(), field: HEAP_ALLOC.to_string() });
        }
        ArrayAllocator::LinuxMmap => {
            out.push(Lir::LoadSlot(size_slot));
            out.push(Lir::SyscallMmap);
        }
    }
    // 与数组相同的失败出口：`NULL` 或负 errno 都不得留成后续故障。
    out.push(Lir::StoreSlot(auxiliary));
    out.push(Lir::LoadI64(0));
    out.push(Lir::StoreSlot(condition));
    out.push(Lir::LoadSlot(auxiliary));
    out.push(Lir::LoadSlotToRcx(condition));
    out.push(Lir::Operator(X86_64OperatorCode::I64CompareGreaterThan));
    out.push(Lir::JumpIfZero(failure_label));
    out.push(Lir::LoadSlot(auxiliary));
    out
}

/// 释放一个由 [`array_allocate`] 建立的数组；`rax` 必须是指针。
/// 只用于宿主桥的一次性副本：guest 值图没有回收通道，后端不释放语言数组。
/// `slot.auxiliary` / `slot.condition` 是周转槽，返回后内容不再有意义。
pub fn array_free(allocator: ArrayAllocator, slot: ArrayAllocationSlots, zero_slot: u16) -> Vec<Lir> {
    let mut out = Vec::new();
    out.push(Lir::StoreSlot(slot.auxiliary));
    match allocator {
        ArrayAllocator::WindowsProcessHeap => {
            out.push(Lir::CallImport { module: KERNEL32_DLL.to_string(), field: GET_PROCESS_HEAP.to_string() });
            out.push(Lir::StoreSlot(slot.condition));
            // `HeapFree(hHeap, 0, lpMem)`。
            out.push(Lir::LoadSlotToArg { slot: slot.condition, index: 0 });
            out.push(Lir::LoadSlotToArg { slot: zero_slot, index: 1 });
            out.push(Lir::LoadSlotToArg { slot: slot.auxiliary, index: 2 });
            out.push(Lir::CallImport { module: KERNEL32_DLL.to_string(), field: HEAP_FREE.to_string() });
        }
        ArrayAllocator::LinuxMmap => {
            // 释放长度来自头部第二格：`munmap(ptr, [ptr + 8])`。
            out.push(Lir::LoadSlot(slot.auxiliary));
            out.push(Lir::LoadFromRaxDisplacement(ARRAY_ALLOCATION_BYTES_OFFSET));
            out.push(Lir::StoreSlot(slot.condition));
            out.push(Lir::LoadSlot(slot.auxiliary));
            out.push(Lir::LoadSlotToRcx(slot.condition));
            out.push(Lir::SyscallMunmap);
        }
    }
    out
}

/// 边界检查：`index` 必须落在 `[0, [array])`，否则跳到 `failure_label`。
///
/// 这是 MIR `ArrayGet/Set` 与 provider `array.get/set` 两个入口共用的唯一定义检查。
/// 检查结束后调用方自行重装访问寄存器。
pub fn array_bounds_check(
    array_slot: u16,
    index_slot: u16,
    condition_slot: u16,
    failure_label: u32,
) -> Vec<Lir> {
    vec![
        Lir::LoadSlot(array_slot),
        Lir::LoadFromRax,
        Lir::StoreSlot(condition_slot),
        Lir::LoadSlot(index_slot),
        Lir::LoadSlotToRcx(condition_slot),
        // 无符号比较：负下标同样越界。
        Lir::Operator(X86_64OperatorCode::I64CompareLessThanUnsigned),
        Lir::JumpIfZero(failure_label),
    ]
}

/// 定义好的数组失败出口：显式陷入，不伪造值、不返回空指针。
pub fn array_failure_exit(label: u32) -> Vec<Lir> {
    vec![Lir::Label(label), Lir::Trap]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_allocate() -> Vec<Lir> {
        array_allocate(
            ArrayAllocator::WindowsProcessHeap,
            ArrayAllocationSlots { count: 0, bytes: 1, auxiliary: 2, condition: 3 },
            9,
        )
    }

    #[test]
    fn layout_is_frozen_for_eight_byte_slots() {
        assert_eq!(ARRAY_HEADER_BYTES, 16);
        assert_eq!(ARRAY_COUNT_OFFSET, 0);
        assert_eq!(ARRAY_ALLOCATION_BYTES_OFFSET, 8);
        assert_eq!(ARRAY_ELEMENT_OFFSET, ARRAY_HEADER_BYTES as i32);
        assert_eq!(ARRAY_ELEMENT_BYTES, ARRAY_ELEMENT_ALIGNMENT);
        assert!(ARRAY_MAX_ELEMENTS > 0);
        assert!(ARRAY_MAX_ELEMENTS.checked_mul(ARRAY_ELEMENT_BYTES).and_then(|bytes| bytes.checked_add(ARRAY_HEADER_BYTES)).is_some());
    }

    #[test]
    fn windows_allocation_routes_through_process_heap_and_checks_failure() {
        let lir = windows_allocate();
        assert!(lir.iter().any(|item| matches!(item, Lir::CallImport { field, .. } if field == GET_PROCESS_HEAP)));
        assert!(lir.iter().any(|item| matches!(item, Lir::CallImport { field, .. } if field == HEAP_ALLOC)));
        assert!(lir.contains(&Lir::JumpIfZero(9)), "范围与分配失败都必须跳到同一个失败出口");
        assert_eq!(lir.last(), Some(&Lir::LoadSlot(2)), "结果指针必须回到 rax");
    }

    #[test]
    fn linux_allocation_uses_mmap_without_host_imports() {
        let lir = array_allocate(
            ArrayAllocator::LinuxMmap,
            ArrayAllocationSlots { count: 0, bytes: 1, auxiliary: 2, condition: 3 },
            9,
        );
        assert!(lir.contains(&Lir::SyscallMmap));
        assert!(!lir.iter().any(|item| matches!(item, Lir::CallImport { .. })), "Linux 数组运行时不得导入 kernel32");
    }

    #[test]
    fn bounds_check_rejects_both_edges_unsigned() {
        let lir = array_bounds_check(0, 1, 2, 9);
        assert!(lir.contains(&Lir::Operator(X86_64OperatorCode::I64CompareLessThanUnsigned)));
        assert_eq!(lir.last(), Some(&Lir::JumpIfZero(9)));
    }

    #[test]
    fn free_matches_the_selected_runner() {
        let slots = ArrayAllocationSlots { count: 0, bytes: 1, auxiliary: 2, condition: 3 };
        let windows = array_free(ArrayAllocator::WindowsProcessHeap, slots, 4);
        assert!(windows.iter().any(|item| matches!(item, Lir::CallImport { field, .. } if field == HEAP_FREE)));
        let linux = array_free(ArrayAllocator::LinuxMmap, slots, 4);
        assert!(linux.contains(&Lir::SyscallMunmap));
        assert!(linux.contains(&Lir::LoadFromRaxDisplacement(ARRAY_ALLOCATION_BYTES_OFFSET)));
    }

    #[cfg(windows)]
    mod windows_execution {
        use super::*;
        use crate::lowering::backends::machine::{X64CallConv, X64ProgramFunction, x64_program::encode_x64_program};
        use data_binary_pe::pe::{NativePeImage, NativePeWriter};
        use std::{fs, process::Command};

        const FAILURE_LABEL: u32 = 9;

        fn assemble(instructions: Vec<Lir>) -> std::path::PathBuf {
            use std::sync::atomic::{AtomicU32, Ordering};
            static SERIAL: AtomicU32 = AtomicU32::new(0);
            let image = encode_x64_program(
                &[X64ProgramFunction { name: "main".to_string(), slot_count: 8, instructions }],
                X64CallConv::Win64,
            )
            .expect("encode array runtime probe");
            let entry = image.symbols.iter().find(|(name, _)| name == "main").expect("main symbol").1;
            let bytes = NativePeWriter::write_executable(&NativePeImage {
                text: image.text,
                rdata: Vec::new(),
                idata: image.idata,
                imports: image.imports,
                exports: Vec::new(),
                text_rel32_fixups: image.text_rel32_fixups,
                is_dll: false,
                entry_point: entry,
            })
            .expect("write PE");
            let serial = SERIAL.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!("nyar-array-runtime-{}-{serial}.exe", std::process::id()));
            fs::write(&path, bytes).expect("write probe exe");
            path
        }

        fn exit_code(path: &std::path::Path) -> i32 {
            let status = Command::new(path).status().expect("run probe exe");
            status.code().unwrap_or(-1)
        }

        /// 真实执行：分配 3 槽、按冻结偏移写读首元素，退出码必须等于写入值。
        #[test]
        fn allocated_array_round_trips_first_element() {
            let mut instructions = vec![Lir::LoadI64(3), Lir::StoreSlot(0), Lir::LoadSlot(0)];
            instructions.extend(array_allocate(
                ArrayAllocator::WindowsProcessHeap,
                ArrayAllocationSlots { count: 1, bytes: 2, auxiliary: 3, condition: 4 },
                FAILURE_LABEL,
            ));
            instructions.extend([
                Lir::StoreSlot(5),
                Lir::LoadI64(42),
                Lir::StoreSlot(6),
                Lir::LoadI64(0),
                Lir::StoreSlot(7),
                Lir::LoadSlotToRcx(7),
                Lir::LoadSlotToRdx(6),
                Lir::LoadSlot(5),
                Lir::StoreIndexedRdxToRaxRcx,
                Lir::LoadSlotToRcx(7),
                Lir::LoadSlot(5),
                Lir::LoadIndexedFromRaxRcx,
                Lir::Return,
            ]);
            instructions.extend(array_failure_exit(FAILURE_LABEL));
            let path = assemble(instructions);
            assert_eq!(exit_code(&path), 42, "数组首元素必须按冻结偏移往返");
            let _ = fs::remove_file(path);
        }

        /// 真实执行：越界访问必须走显式失败出口（进程异常终止），而不是读到相邻内存。
        #[test]
        fn out_of_bounds_takes_the_defined_failure_exit() {
            let mut checked = vec![Lir::LoadI64(2), Lir::StoreSlot(0), Lir::LoadSlot(0)];
            checked.extend(array_allocate(
                ArrayAllocator::WindowsProcessHeap,
                ArrayAllocationSlots { count: 1, bytes: 2, auxiliary: 3, condition: 4 },
                FAILURE_LABEL,
            ));
            checked.extend([
                Lir::StoreSlot(5),
                Lir::LoadI64(2),
                Lir::StoreSlot(6),
                Lir::LoadSlotToRcx(6),
                Lir::LoadSlot(5),
                Lir::StoreSlot(7),
            ]);
            checked.extend(array_bounds_check(7, 6, 4, FAILURE_LABEL));
            checked.extend([Lir::LoadSlotToRcx(6), Lir::LoadSlot(7), Lir::LoadIndexedFromRaxRcx, Lir::Return]);
            checked.extend(array_failure_exit(FAILURE_LABEL));
            let path = assemble(checked);
            assert_ne!(exit_code(&path), 0, "越界不得作为正常返回");
            let _ = fs::remove_file(path);
        }

        /// 真实执行：零长数组只分配头部，长度读到 0。
        #[test]
        fn zero_length_array_has_zero_length_header() {
            let mut instructions = vec![Lir::LoadI64(0), Lir::StoreSlot(0), Lir::LoadSlot(0)];
            instructions.extend(array_allocate(
                ArrayAllocator::WindowsProcessHeap,
                ArrayAllocationSlots { count: 1, bytes: 2, auxiliary: 3, condition: 4 },
                FAILURE_LABEL,
            ));
            instructions.extend([Lir::LoadFromRax, Lir::Return]);
            instructions.extend(array_failure_exit(FAILURE_LABEL));
            let path = assemble(instructions);
            assert_eq!(exit_code(&path), 0, "零长数组的长度必须是 0");
            let _ = fs::remove_file(path);
        }

        /// 真实执行：注入分配失败（巨大但未溢出的容量）必须走定义失败出口。
        #[test]
        fn allocation_failure_takes_the_defined_failure_exit() {
            // `1 << 50` 个 8 字节槽 = 8 PiB：范围检查通过，分配实现必然失败。
            let mut instructions = vec![Lir::LoadI64(1 << 50), Lir::StoreSlot(0), Lir::LoadSlot(0)];
            instructions.extend(array_allocate(
                ArrayAllocator::WindowsProcessHeap,
                ArrayAllocationSlots { count: 1, bytes: 2, auxiliary: 3, condition: 4 },
                FAILURE_LABEL,
            ));
            instructions.push(Lir::Return);
            instructions.extend(array_failure_exit(FAILURE_LABEL));
            let path = assemble(instructions);
            assert_ne!(exit_code(&path), 0, "分配失败不得作为正常返回");
            let _ = fs::remove_file(path);
        }

        /// 真实执行：分配→释放后进程正常结束，释放实现与分配实现匹配。
        #[test]
        fn free_after_allocation_returns_cleanly() {
            let mut instructions = vec![Lir::LoadI64(4), Lir::StoreSlot(0), Lir::LoadSlot(0)];
            instructions.extend(array_allocate(
                ArrayAllocator::WindowsProcessHeap,
                ArrayAllocationSlots { count: 1, bytes: 2, auxiliary: 3, condition: 4 },
                FAILURE_LABEL,
            ));
            instructions.push(Lir::StoreSlot(5));
            instructions.push(Lir::LoadI64(0));
            instructions.push(Lir::StoreSlot(6));
            instructions.push(Lir::LoadSlot(5));
            instructions.extend(array_free(
                ArrayAllocator::WindowsProcessHeap,
                ArrayAllocationSlots { count: 1, bytes: 2, auxiliary: 3, condition: 4 },
                6,
            ));
            instructions.extend([Lir::LoadI64(42), Lir::Return]);
            instructions.extend(array_failure_exit(FAILURE_LABEL));
            let path = assemble(instructions);
            assert_eq!(exit_code(&path), 42, "释放后必须能正常返回");
            let _ = fs::remove_file(path);
        }
    }
}
