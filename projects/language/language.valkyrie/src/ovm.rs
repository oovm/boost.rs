//! `.odin` 产物导出枚举与带 ABI 合同的调用。
//!
//! 结构物化只允许按与产物字节一一对应的 ABI sidecar 合同进行；sidecar 缺失、
//! 解析失败或哈希不符时失败闭合，不按猜测布局解释堆内存。

use std::{fs, path::Path};

use data_binary_odin::{OvmAbiSidecar, ovm_abi_sidecar_file};
use interpreter_vm_ovm::{OdinVm, json_bridge};

/// 列出 `.odin` 模块的导出符号。
///
/// # Errors
///
/// 读取或加载模块失败时返回错误描述。
pub fn list_exports(module: &Path) -> Result<Vec<String>, String> {
    let bytes = fs::read(module).map_err(|error| format!("read module: {error}"))?;
    let loaded = OdinVm::new().load(&bytes).map_err(|error| format!("load module: {error}"))?;
    Ok(loaded.exports.iter().map(|export| export.symbol_name.clone()).collect())
}

/// 调用 `.odin` 导出并返回 JSON 结果（`args_json` 为 JSON 数组）。
///
/// 产物旁的 ABI sidecar 缺失、解析失败或与产物哈希不符时拒绝调用。
///
/// # Errors
///
/// 读取产物、ABI 合同校验、参数解析或执行失败时返回错误描述。
pub fn invoke_export(module: &Path, entry: &str, args_json: &str) -> Result<String, String> {
    let bytes = fs::read(module).map_err(|error| format!("read module: {error}"))?;
    let sidecar = load_abi_sidecar(module, &bytes)?;
    let export = sidecar.contract.export(entry).ok_or_else(|| format!("OVM ABI 合同没有导出 `{entry}`；拒绝按未声明的结构物化参数/返回值"))?;
    let mut vm = OdinVm::new();
    let loaded = vm.load(&bytes).map_err(|error| format!("load module: {error}"))?;
    let args = json_bridge::parse_call_args_json_with_contract(args_json, export, vm.heap_mut())
        .map_err(|error| format!("parse args-json: {error}"))?;
    let result = vm.run(&loaded, entry, args).map_err(|error| format!("run entry `{entry}`: {error}"))?;
    let json =
        json_bridge::value_to_json_with_contract(&result, &export.returns, vm.heap()).map_err(|error| format!("serialize result: {error}"))?;
    serde_json::to_string(&json).map_err(|error| format!("encode result json: {error}"))
}

fn load_abi_sidecar(module: &Path, artifact_bytes: &[u8]) -> Result<OvmAbiSidecar, String> {
    let artifact = module.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let sidecar_path = module.with_file_name(ovm_abi_sidecar_file(artifact));
    let sidecar_bytes = fs::read(&sidecar_path)
        .map_err(|error| format!("OVM ABI 合同缺失：`{}`（{error}）；拒绝在无合同的情况下物化结构参数/返回值", sidecar_path.display()))?;
    let sidecar: OvmAbiSidecar =
        serde_json::from_slice(&sidecar_bytes).map_err(|error| format!("解析 OVM ABI 合同 `{}`: {error}", sidecar_path.display()))?;
    sidecar.verify_against(artifact_bytes).map_err(|error| format!("校验 OVM ABI 合同 `{}`: {error}", sidecar_path.display()))?;
    Ok(sidecar)
}
