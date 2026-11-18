#![deny(missing_docs)]
#![forbid(unsafe_code)]

//! Odin bytecode VM — the `.odin` target under the `interpreter.vm` tree.
//!
//! # Tree position
//!
//! `interpreter.vm` holds per-target VM execution substrates. This crate is its Odin target;
//! sibling targets (for example a wasm or JVM bytecode VM) get their own `interpreter.vm.*`
//! crate and must not be folded into this one.
//!
//! # Layering
//!
//! - This crate executes **decoded Odin IR modules** only. It must not import concrete
//!   language ASTs or compilers.
//! - Managed heap / GC live in Boost **`interpreter-gc`**; the optional JIT substrate lives in Boost
//!   **`interpreter-jit`**, with the Odin bytecode → scalar-blob matcher kept here as the format adapter.
//! - This crate owns the interpreter loop.
//! - Concrete languages lower through a compiler emitter into `.odin` bytes before this VM sees them.
//!
//! # Naming
//!
//! Crate package: `interpreter-vm-ovm`. Rust import path: `interpreter_vm_ovm`.

pub mod array_runtime;
pub mod deopt_value;
pub mod dom_runtime;
pub mod error;
pub mod executable;
pub mod executor;
pub mod frame;
pub mod host;
pub mod jit;
pub mod json_bridge;
pub mod module;
pub mod odin_scalar_matcher;
pub mod ops;
pub mod output_runtime;
pub mod random_runtime;
pub mod scalar_runtime;
pub mod stack;
pub mod text_runtime;
pub mod time_runtime;
pub mod value;
pub mod verify;
pub mod vm;
pub mod workload_json;

pub use crate::{
    deopt_value::{decode_value_from_deopt, decode_value_from_deopt_with_roots, encode_value_for_deopt, encode_value_for_deopt_pinned},
    error::OdinRuntimeError,
    executable::{ExecOp, ExecutableFunction, InstructionIndex},
    executor::ExecutionOutcome,
    module::ModuleGlobals,
    scalar_runtime::{execute_scalar_blob, execute_scalar_program},
    value::{CoroutineState, ObjectId, Value},
    vm::OdinVm,
};
pub use interpreter_gc::{
    ConcurrentMarkController, ConcurrentMarkError, ConcurrentMarkEvent, ConcurrentMarkState, ConcurrentMarkTicker, GarbageCollector, GcMode,
    GcPolicy, GcRoots, Generation, HostRoots, IntentError, IntentSource, LayoutDescriptor, LayoutId, ObjectHeap, ObjectLifetimeHint,
    ObjectPayload, PromotionFailure, RelocateMap, RootHandle, RootHandshakeReport, StrategyController, StrategyDecision, StrategyTransition,
    TracePollReport, WorkloadHints, WorkloadIntent, WriteBarrier,
};
