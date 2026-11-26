//! TS std provider：`odin.host` 模块下内核未收录的 `ts.*` 符号的宿主实现。
//!
//! 语义与 [`super::semantics`] 完全同源——本文件只做 VM 值域与语言值域的
//! 边界转换，绝不出现第二份语义实现。provider 缺席时含 `ts.*` 导入的产物
//! 在装载期失败闭合（这是语言 provider 缺失的结构化失败路径）。

use std::sync::{Arc, Mutex};

use interpreter_vm_ovm::{
    error::OdinRuntimeError,
    host::{HostImport, HostProvider},
    ObjectHeap, Value as VmValue,
};

use super::{semantics, value::Value};

/// `console.log` 行的可取 trace（宿主注入，保持调用顺序）。
pub type Trace = Arc<Mutex<Vec<String>>>;

/// TypeScript 标准库 provider。
#[derive(Debug, Clone)]
pub struct TsStdProvider {
    trace: Trace,
}

impl TsStdProvider {
    /// 以宿主提供的 trace 槽构造 provider。
    pub fn new(trace: Trace) -> Self {
        Self { trace }
    }
}

impl HostProvider for TsStdProvider {
    fn resolve(&self, module: &str, symbol: &str) -> Option<Arc<dyn HostImport>> {
        if module != "odin.host" {
            return None;
        }
        match symbol {
            "ts.add" => Some(Arc::new(TsAdd)),
            "ts.truthy" => Some(Arc::new(TsTruthy)),
            "ts.strictEqual" => Some(Arc::new(TsStrictEq)),
            "ts.num_to_string" => Some(Arc::new(TsNumToString)),
            "ts.log" => Some(Arc::new(TsLog { trace: self.trace.clone() })),
            _ => None,
        }
    }
}

/// VM 标量 → 语言值域（对象 / 协程不在此转换内）。
fn scalar_from_vm(value: &VmValue) -> Option<Value> {
    Some(match value {
        VmValue::Null => Value::Null,
        VmValue::Bool(flag) => Value::Bool(*flag),
        VmValue::I32(number) => Value::Number(*number as f64),
        VmValue::I64(number) => Value::Number(*number as f64),
        VmValue::F32(number) => Value::Number(*number as f64),
        VmValue::F64(number) => Value::Number(*number),
        VmValue::String(text) => Value::string(text.as_str()),
        _ => return None,
    })
}

/// 语言标量 → VM 值域（对象值域不跨边界）。
fn scalar_to_vm(value: &Value) -> Option<VmValue> {
    Some(match value {
        Value::Undefined | Value::Null => VmValue::Null,
        Value::Bool(flag) => VmValue::Bool(*flag),
        Value::Number(number) => VmValue::F64(*number),
        Value::String(text) => VmValue::String(text.to_string()),
        _ => return None,
    })
}

/// 单参数校验。
fn arity(args: &[VmValue], expected: usize, symbol: &str) -> Result<(), OdinRuntimeError> {
    if args.len() != expected {
        return Err(OdinRuntimeError::ModuleLoad(format!("`{symbol}` 期望 {expected} 个实参，收到 {}", args.len())));
    }
    Ok(())
}

fn type_mismatch(value: &VmValue, symbol: &str) -> OdinRuntimeError {
    OdinRuntimeError::TypeMismatch { expected: "ts scalar value", actual: format!("{symbol}: {}", value.type_name()) }
}

/// `ts.add(a, b)`：JS `+` 语义。
#[derive(Debug)]
struct TsAdd;

impl HostImport for TsAdd {
    fn call(&self, args: &[VmValue], _heap: &mut ObjectHeap) -> Result<VmValue, OdinRuntimeError> {
        arity(args, 2, "ts.add")?;
        let left = scalar_from_vm(&args[0]).ok_or_else(|| type_mismatch(&args[0], "ts.add"))?;
        let right = scalar_from_vm(&args[1]).ok_or_else(|| type_mismatch(&args[1], "ts.add"))?;
        scalar_to_vm(&semantics::add(&left, &right)).ok_or_else(|| type_mismatch(&args[0], "ts.add"))
    }
}

/// `ts.truthy(v)`：JS truthiness（对象恒真）。
#[derive(Debug)]
struct TsTruthy;

impl HostImport for TsTruthy {
    fn call(&self, args: &[VmValue], _heap: &mut ObjectHeap) -> Result<VmValue, OdinRuntimeError> {
        arity(args, 1, "ts.truthy")?;
        Ok(VmValue::Bool(match &args[0] {
            VmValue::Object(_) => true,
            other => semantics::truthy(&scalar_from_vm(other).ok_or_else(|| type_mismatch(other, "ts.truthy"))?),
        }))
    }
}

/// `ts.strictEqual(a, b)`：JS `===`（对象按 `ObjectId` 身份）。
#[derive(Debug)]
struct TsStrictEq;

impl HostImport for TsStrictEq {
    fn call(&self, args: &[VmValue], _heap: &mut ObjectHeap) -> Result<VmValue, OdinRuntimeError> {
        arity(args, 2, "ts.strictEqual")?;
        let flag = match (&args[0], &args[1]) {
            (VmValue::Object(left), VmValue::Object(right)) => left == right,
            (VmValue::Object(_), _) | (_, VmValue::Object(_)) => false,
            (left, right) => {
                let left = scalar_from_vm(left).ok_or_else(|| type_mismatch(left, "ts.strictEqual"))?;
                let right = scalar_from_vm(right).ok_or_else(|| type_mismatch(right, "ts.strictEqual"))?;
                semantics::strict_eq(&left, &right)
            }
        };
        Ok(VmValue::Bool(flag))
    }
}

/// `ts.num_to_string(v)`：JS `ToString`（文本拼接路径的数值格式化）。
#[derive(Debug)]
struct TsNumToString;

impl HostImport for TsNumToString {
    fn call(&self, args: &[VmValue], _heap: &mut ObjectHeap) -> Result<VmValue, OdinRuntimeError> {
        arity(args, 1, "ts.num_to_string")?;
        let value = scalar_from_vm(&args[0]).ok_or_else(|| type_mismatch(&args[0], "ts.num_to_string"))?;
        Ok(VmValue::String(semantics::to_string(&value)))
    }
}

/// `ts.log(...)`：按参数空格连接写入 trace，保持调用序。
#[derive(Debug)]
struct TsLog {
    trace: Trace,
}

impl HostImport for TsLog {
    fn call(&self, args: &[VmValue], _heap: &mut ObjectHeap) -> Result<VmValue, OdinRuntimeError> {
        let mut parts = Vec::with_capacity(args.len());
        for arg in args {
            parts.push(match arg {
                VmValue::Object(_) => "[object Object]".to_string(),
                other => semantics::to_string(&scalar_from_vm(other).ok_or_else(|| type_mismatch(other, "ts.log"))?),
            });
        }
        self.trace.lock().expect("trace 锁不中毒").push(parts.join(" "));
        Ok(VmValue::Null)
    }
}
