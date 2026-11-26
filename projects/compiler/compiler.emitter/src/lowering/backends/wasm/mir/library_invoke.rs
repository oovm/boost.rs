//! 库模式 JSON ABI：消费 `WasmAbiType` 结构化合同，序列化到
//! `nyar.library_invoke` 自定义段。
//!
//! 合同版本固定为 `0`。本模块只描述当前合同，不保留版本迁移叙述，也不提供
//! 旧形状的兼容路径。
//!
//! `exports[*].params` 是**参数身份对象数组**：`index`（声明顺序，0 起连续）、
//! `name`（来自 compiler schema 的 canonical 参数名，禁止按 metadata / 类型名
//! 猜测）、`type`（`WasmAbiType` JSON）与 `carrier`（物理载体：`i32` / `i64` /
//! `f32` / `f64` / `void` / `handle`）。参数名缺失、重复，或参数身份数量与类型
//! 数量不一致都会在此结构化失败。
//!
//! # JSON 格式
//!
//! ```json
//! {
//!   "version": 0,
//!   "exports": {
//!     "twoSum": {
//!       "params": [
//!         {"index": 0, "name": "nums", "type": {"kind": "I64"}, "carrier": "i64"}
//!       ],
//!       "returns": {"kind": "I64"}
//!     }
//!   },
//!   "abi_digest": "sha256-<hex>"
//! }
//! ```

use compiler_types::combined_content_hash;

use crate::{
    Result,
    emitter_backend_wasi::{WasmBinaryModule, WasmPackageKind, WasmSection},
};

use super::{WasmFrozenExportAbi, WasmFunctionInput, WasmModuleInput, canonical_parameter_identities};

/// `nyar.library_invoke` JSON schema 版本（`/v0`）。
///
/// 跨仓消费者（如 `leetcode.v` 的 `abi.ts`）据此校验协议版本；版本变化同时会
/// 改变 [`abi_contract_digest`]。参数身份为 `{index, name, type, carrier}`，
/// 不提供旧形状的兼容路径。
const LIBRARY_INVOKE_SCHEMA_VERSION: u64 = 0;

/// 从 `WasmAbiType` schema 推导物理载体名称。
///
/// 本函数**只用于与 plan 冻结载体交叉核对**，不是载体事实源；`carrier` 字段
/// 只能取 [`LibraryExportAbi`] 的冻结值。标量按 Wasm 原生槽位，容器（数组 /
/// Option / Tuple / 具名聚合）与引用存储名义实例（`InstanceHandle`，共享 GC
/// 引用）统一走 `handle`。`Unsupported` 不应出现在参数位，出现即失败。
fn abi_carrier_name(schema: &super::abi_schema::WasmAbiType) -> Result<&'static str> {
    use super::abi_schema::WasmAbiType;
    Ok(match schema {
        WasmAbiType::I32 => "i32",
        WasmAbiType::I64 => "i64",
        WasmAbiType::F32 => "f32",
        WasmAbiType::F64 => "f64",
        // Bool / Char 在 Wasm 层统一用 i32 槽位。
        WasmAbiType::Bool | WasmAbiType::Char => "i32",
        // Utf8 在 JS glue 轨是 host-intern i32 句柄。
        WasmAbiType::Utf8 => "i32",
        WasmAbiType::Unit => "void",
        WasmAbiType::InstanceHandle { .. } => "handle",
        WasmAbiType::Array { .. }
        | WasmAbiType::FixedArray { .. }
        | WasmAbiType::Option { .. }
        | WasmAbiType::Tuple { .. }
        | WasmAbiType::NamedAggregate { .. }
        | WasmAbiType::Sum { .. } => "handle",
        WasmAbiType::Utf16 => {
            return Err(crate::lowering::features::physical_contract::UTF16_UNRESOLVED_TEXT_ABI.to_string());
        }
        WasmAbiType::Unsupported { reason } => return Err(format!("类型没有 Wasm ABI 载体: {reason}")),
    })
}

/// 消费适配器结算期冻结的 `WasmAbiType` schema，产出带参数身份的 `params`
/// JSON 条目。编码层不再现场重走 ABI 派生。
///
/// `carrier` 字段只取冻结合同中的载体值；`abi_carrier_name` 仅用来做
/// 交叉核对，冻结载体与冻结 schema 不一致即结构化失败。任何 `Unsupported`
/// 变体会立刻返回可读错误，**禁止**静默降级。
/// `instance_label` 仅用于错误信息（可以是 instance ID 的 Display 或公开名）。
fn schematize_function_abi(
    instance_label: &str,
    function: &WasmFunctionInput,
    frozen: &WasmFrozenExportAbi,
) -> Result<(Vec<serde_json::Value>, super::abi_schema::WasmAbiType)> {
    let identities = canonical_parameter_identities(instance_label, function)?;
    let abi = &frozen.carriers;
    if abi.params.len() != function.param_types.len() {
        return Err(format!(
            "库导出 `{instance_label}` 的冻结 ABI 参数载体数量与参数类型数量不一致: plan_params={} param_types={}",
            abi.params.len(),
            function.param_types.len()
        ));
    }
    if frozen.params.len() != function.param_types.len() {
        return Err(format!(
            "库导出 `{instance_label}` 的冻结 ABI 参数 schema 数量与参数类型数量不一致: schema_params={} param_types={}",
            frozen.params.len(),
            function.param_types.len()
        ));
    }
    let mut params_json = Vec::with_capacity(function.param_types.len());
    for (position, ty) in function.param_types.iter().enumerate() {
        let schema = &frozen.params[position];
        if let Some(reason) = schema.unsupported_reason() {
            return Err(format!("库导出 `{instance_label}` 参数 {position} 类型 `{ty}` 没有 Wasm ABI 合同: {reason}"));
        }
        let (index, name) = &identities[position];
        let carrier = abi.params[position];
        let schema_carrier =
            abi_carrier_name(schema).map_err(|reason| format!("库导出 `{instance_label}` 参数 {index} `{name}` 的 {reason}"))?;
        if schema_carrier != carrier.as_str() {
            return Err(format!(
                "库导出 `{instance_label}` 参数 {index} `{name}` 的冻结载体与 Wasm ABI schema 不一致: \
                 plan_carrier={} schema_carrier={schema_carrier}",
                carrier.as_str()
            ));
        }
        params_json.push(serde_json::json!({
            "index": index,
            "name": name,
            "type": schema,
            "carrier": carrier.as_str(),
        }));
    }

    let ret_schema = frozen.returns.clone();
    if let Some(reason) = ret_schema.unsupported_reason() {
        return Err(format!("库导出 `{instance_label}` 返回类型 `{}` 没有 Wasm ABI 合同: {reason}", function.return_type));
    }
    let schema_carrier =
        abi_carrier_name(&ret_schema).map_err(|reason| format!("库导出 `{instance_label}` 返回类型 `{}` 的 {reason}", function.return_type))?;
    if schema_carrier != abi.returns.as_str() {
        return Err(format!(
            "库导出 `{instance_label}` 返回值的冻结载体与 Wasm ABI schema 不一致: plan_carrier={} schema_carrier={schema_carrier}",
            abi.returns.as_str()
        ));
    }
    Ok((params_json, ret_schema))
}

/// 把库模式导出 ABI 合同序列化进 `nyar.library_invoke` 自定义段。
pub(super) fn append_library_invoke_section(
    wasm_package_kind: WasmPackageKind,
    input: &WasmModuleInput,
    module: &mut WasmBinaryModule,
) -> Result<()> {
    if wasm_package_kind != WasmPackageKind::Library {
        return Ok(());
    }
    let mut invoke_exports = serde_json::Map::new();

    // wasm 的命名合同：有 `name:` / `case:` 注解用注解名，否则用源码名。
    for instance in input.export_names.keys() {
        let public_name = input.export_public_name(instance).expect("导出实例必有公开名").clone();
        let function = input.function(instance).ok_or_else(|| format!("库导出 `{instance}` 缺少 Compiler 函数体"))?;
        let Some(frozen) = input.export_abis.get(instance)
        else {
            return Err(format!("库导出 `{instance}`（公开名 `{public_name}`）缺少适配器结算提交的冻结 ABI 合同；backend 不得自行推导 carrier"));
        };

        let instance_label = format!("{instance}");
        let (params_json, returns_schema) = schematize_function_abi(&instance_label, function, frozen)?;

        // 注意：WasmAbiType 的 serde(tag = "kind", rename_all = "PascalCase")
        // 会自动生成 {"kind": "I64"} / {"kind": "Array", "element": {...}} 格式。
        if invoke_exports
            .insert(
                public_name.clone(),
                serde_json::json!({
                    "params": params_json,
                    "returns": returns_schema,
                }),
            )
            .is_some()
        {
            return Err(format!("库导出公开名 `{public_name}` 重复"));
        }
    }
    if invoke_exports.is_empty() {
        return Err("库 Wasm 产物没有可表达的导出合同".to_string());
    }

    let abi_digest = abi_contract_digest(LIBRARY_INVOKE_SCHEMA_VERSION, &serde_json::Value::Object(invoke_exports.clone()));
    let payload = serde_json::json!({
        "version": LIBRARY_INVOKE_SCHEMA_VERSION,
        "exports": invoke_exports,
        "abi_digest": abi_digest,
    });

    module.sections.push(WasmSection { id: 0, name: Some("nyar.library_invoke".to_owned()), bytes: payload.to_string().into_bytes() });
    Ok(())
}

/// 规范化 JSON 序列化：对象键按字节序排序，数组保持原序，标量沿用 JSON 文本。
///
/// `serde_json::Map` 默认是 `BTreeMap`，但 map 顺序可能随 feature 变化；这里显式
/// 排序，保证 digest 只由 ABI 事实决定。
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            let body = entries
                .iter()
                .map(|(key, value)| format!("{}:{}", serde_json::Value::String((*key).clone()), canonical_json(value)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        serde_json::Value::Array(items) => {
            format!("[{}]", items.iter().map(canonical_json).collect::<Vec<_>>().join(","))
        }
        other => other.to_string(),
    }
}

/// 计算 `nyar.library_invoke` 的稳定 ABI digest。
///
/// 对 `version` 与 `exports` 的规范化 JSON 做 SHA-256：同一 ABI 事实恒得同一
/// digest，协议版本或任一导出签名变化都会改变 digest。
fn abi_contract_digest(version: u64, exports: &serde_json::Value) -> String {
    let facts = serde_json::json!({
        "version": version,
        "exports": exports,
    });
    format!("sha256-{}", combined_content_hash(&[&canonical_json(&facts)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lowering::backends::wasm::abi::{WasmAbiCarrier, WasmLibraryExportAbi};
    use crate::lowering::backends::wasm::mir::abi_schema::abi_schema_for_type;
    use crate::lowering::backends::wasm::mir::wasm_export_abi;
    use crate::lowering::backends::wasm::mir::{
        WasmBlock, WasmBlockKey, WasmCallableKey, WasmFieldFact, WasmLayoutFact, WasmStorageClass, WasmTerminator, WasmTypeFact, WasmValue,
        WasmValueKey, WasmValueOrigin,
    };

    fn named_type(spelling: &str) -> WasmTypeFact {
        match spelling {
            "bool" => WasmTypeFact::Boolean,
            "i32" => WasmTypeFact::Integer32 { signed: true },
            "i64" => WasmTypeFact::Integer64 { signed: true },
            "f32" => WasmTypeFact::Float32,
            "f64" => WasmTypeFact::Float64,
            "char" => WasmTypeFact::Character,
            "utf8" => WasmTypeFact::Utf8,
            "unit" => WasmTypeFact::Unit,
            other => panic!("测试只支持 canonical 原语拼写: {other}"),
        }
    }

    // ---- ABI digest ----

    #[test]
    fn abi_digest_is_independent_of_object_key_order() {
        let exports_a: serde_json::Value = serde_json::from_str(
            r#"{"solve":{"params":[{"kind":"I64"}],"returns":{"kind":"Unit"}},"build":{"params":[],"returns":{"kind":"I32"}}}"#,
        )
        .unwrap();
        let exports_b: serde_json::Value = serde_json::from_str(
            r#"{"build":{"returns":{"kind":"I32"},"params":[]},"solve":{"returns":{"kind":"Unit"},"params":[{"kind":"I64"}]}}"#,
        )
        .unwrap();
        let digest_a = abi_contract_digest(LIBRARY_INVOKE_SCHEMA_VERSION, &exports_a);
        let digest_b = abi_contract_digest(LIBRARY_INVOKE_SCHEMA_VERSION, &exports_b);
        assert!(digest_a.starts_with("sha256-"), "digest should be prefixed: {digest_a}");
        assert_eq!(digest_a, digest_b, "同一 ABI 事实必须得到同一 digest");
    }

    #[test]
    fn abi_digest_changes_with_schema_version() {
        let exports: serde_json::Value = serde_json::from_str(r#"{"solve":{"params":[],"returns":{"kind":"I64"}}}"#).unwrap();
        let version_one = abi_contract_digest(1, &exports);
        let version_two = abi_contract_digest(2, &exports);
        assert_ne!(version_one, version_two, "schema 版本必须参与 digest 规范化输入");
    }

    #[test]
    fn canonical_json_sorts_object_keys() {
        let left: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let right: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(canonical_json(&left), canonical_json(&right));
        assert_eq!(canonical_json(&left), r#"{"a":1,"b":2}"#);
    }

    // ---- 参数身份合同 ----

    /// 构造带 `WasmValueOrigin::Parameter` 身份的测试函数。
    fn abi_function(parameters: &[(usize, &str, WasmTypeFact)], return_type: WasmTypeFact) -> WasmFunctionInput {
        WasmFunctionInput {
            symbol: "identity".to_string(),
            return_type,
            param_types: parameters.iter().map(|(_, _, ty)| ty.clone()).collect(),
            value_types: std::collections::BTreeMap::new(),
            value_layouts: std::collections::BTreeMap::new(),
            return_layout: None,
            entry: WasmBlockKey(0),
            values: parameters
                .iter()
                .map(|(index, name, _)| WasmValue {
                    id: WasmValueKey(u32::try_from(*index).expect("测试参数下标")),
                    origin: WasmValueOrigin::Parameter { index: *index, name: (*name).to_string() },
                })
                .collect(),
            blocks: vec![WasmBlock {
                id: WasmBlockKey(0),
                label: "entry".to_string(),
                parameters: Vec::new(),
                instructions: Vec::new(),
                terminator: WasmTerminator::Return { value: None },
            }],
            parameter_identities: parameters.iter().map(|(index, name, _)| (*index, (*name).to_string())).collect(),
        }
    }

    /// 闭合泛型实例 `ArrayList<i64>` 结算后的形态：布局引用。
    fn arraylist_layout_ref() -> WasmTypeFact {
        WasmTypeFact::Layout(7)
    }

    /// 适配器结算提交的冻结布局事实（`ArrayList<i64>` 实例）。
    fn arraylist_layout() -> WasmLayoutFact {
        WasmLayoutFact {
            id: 7,
            name: "ArrayList".to_string(),
            namespace: "std.collections".to_string(),
            key: None,
            storage: WasmStorageClass::Value,
            size: 8,
            align: 4,
            fields: vec![
                WasmFieldFact { name: "items".to_string(), ty: WasmTypeFact::Array(Box::new(named_type("i64"))), offset: 0, size: 4, align: 4 },
                WasmFieldFact { name: "len".to_string(), ty: named_type("i32"), offset: 4, size: 4, align: 4 },
            ],
        }
    }

    fn list_input() -> WasmModuleInput {
        let mut input = WasmModuleInput::testing_empty();
        input.layouts = vec![arraylist_layout()];
        input
    }

    /// 构造适配器冻结的导出 ABI 合同：载体 + 逐位置 schema。
    fn frozen_abi(params: Vec<WasmAbiCarrier>, returns: WasmAbiCarrier, input: &WasmModuleInput, param_types: &[WasmTypeFact], return_type: &WasmTypeFact) -> WasmFrozenExportAbi {
        WasmFrozenExportAbi {
            carriers: WasmLibraryExportAbi { params, returns },
            params: param_types.iter().map(|ty| abi_schema_for_type(input, ty)).collect(),
            returns: abi_schema_for_type(input, return_type),
        }
    }

    #[test]
    fn schematize_renders_parameter_identity_carrier_and_type() {
        let input = list_input();
        let list = arraylist_layout_ref();
        let function = abi_function(&[(0, "nums", list.clone()), (1, "target", named_type("i64"))], list.clone());
        let abi = frozen_abi(vec![WasmAbiCarrier::Handle, WasmAbiCarrier::I64], WasmAbiCarrier::Handle, &input, &function.param_types, &function.return_type);
        let (params_json, returns) = schematize_function_abi("twoSum", &function, &abi).expect("参数身份完整");
        assert_eq!(params_json[0]["index"], serde_json::json!(0));
        assert_eq!(params_json[0]["name"], serde_json::json!("nums"));
        assert_eq!(params_json[0]["carrier"], serde_json::json!("handle"));
        assert_eq!(params_json[0]["type"]["kind"], serde_json::json!("NamedAggregate"));
        assert_eq!(params_json[0]["type"]["name"], serde_json::json!("ArrayList"));
        assert_eq!(params_json[0]["type"]["module_path"], serde_json::json!("std::collections"));
        assert_eq!(params_json[0]["type"]["fields"].as_array().map(Vec::len), Some(2));
        assert_eq!(params_json[1], serde_json::json!({"index": 1, "name": "target", "type": {"kind": "I64"}, "carrier": "i64"}));
        assert_eq!(returns, abi_schema_for_type(&input, &list));
    }

    #[test]
    fn schematize_rejects_duplicate_parameter_names() {
        let input = WasmModuleInput::testing_empty();
        let function = abi_function(&[(0, "value", named_type("i64")), (1, "value", named_type("i64"))], named_type("i64"));
        let abi = frozen_abi(vec![WasmAbiCarrier::I64, WasmAbiCarrier::I64], WasmAbiCarrier::I64, &input, &function.param_types, &function.return_type);
        let error = schematize_function_abi("identity", &function, &abi).expect_err("参数名重复必须失败");
        assert!(error.contains("identity"), "error must carry the export name: {error}");
        assert!(error.contains("index=1") || error.contains("参数 1"), "error must carry the parameter index: {error}");
        assert!(error.contains("重复"), "error must explain the duplicate name: {error}");
    }

    #[test]
    fn schematize_rejects_missing_parameter_name() {
        let input = WasmModuleInput::testing_empty();
        let function = abi_function(&[(0, "", named_type("i64"))], named_type("i64"));
        let abi = frozen_abi(vec![WasmAbiCarrier::I64], WasmAbiCarrier::I64, &input, &function.param_types, &function.return_type);
        let error = schematize_function_abi("identity", &function, &abi).expect_err("参数名缺失必须失败");
        assert!(error.contains("identity"), "error must carry the export name: {error}");
        assert!(error.contains("缺少 canonical 参数名"), "error must explain the missing name: {error}");
    }

    #[test]
    fn schematize_rejects_identity_count_mismatch() {
        let input = WasmModuleInput::testing_empty();
        let mut function = abi_function(&[(0, "value", named_type("i64"))], named_type("i64"));
        function.param_types.push(named_type("i32"));
        let abi = frozen_abi(vec![WasmAbiCarrier::I64, WasmAbiCarrier::I32], WasmAbiCarrier::I64, &input, &function.param_types, &function.return_type);
        let error = schematize_function_abi("identity", &function, &abi).expect_err("身份数量不一致必须失败");
        assert!(error.contains("数量不一致"), "error must explain the count mismatch: {error}");
    }

    #[test]
    fn schematize_rejects_schema_count_mismatch() {
        let input = WasmModuleInput::testing_empty();
        let function = abi_function(&[(0, "value", named_type("i64"))], named_type("i64"));
        let mut abi = frozen_abi(vec![WasmAbiCarrier::I64], WasmAbiCarrier::I64, &input, &function.param_types, &function.return_type);
        abi.params.pop();
        let error = schematize_function_abi("identity", &function, &abi).expect_err("冻结 schema 数量不一致必须失败");
        assert!(error.contains("schema 数量与参数类型数量不一致"), "error must explain the schema count mismatch: {error}");
    }

    #[test]
    fn schematize_rejects_carrier_disagreement_with_plan() {
        // 冻结载体为 Handle，但冻结 schema 是标量 i32：交叉核对必须失败，
        // backend 不得以任一侧结果覆盖另一侧事实。
        let input = WasmModuleInput::testing_empty();
        let function = abi_function(&[(0, "value", named_type("i32"))], named_type("i32"));
        let mut abi = frozen_abi(vec![WasmAbiCarrier::I64], WasmAbiCarrier::I32, &input, &function.param_types, &function.return_type);
        abi.carriers.params = vec![WasmAbiCarrier::Handle];
        let error = schematize_function_abi("identity", &function, &abi).expect_err("冻结核对不一致必须失败");
        assert!(error.contains("identity"), "error must carry the instance label: {error}");
        assert!(error.contains("参数 0"), "error must carry the parameter index: {error}");
        assert!(error.contains("plan_carrier=handle"), "error must carry the plan carrier: {error}");
        assert!(error.contains("schema_carrier=i32"), "error must carry the schema carrier: {error}");
    }

    #[test]
    fn wasm_freezes_library_export_abi_carriers() {
        let list = arraylist_layout_ref();
        let function = abi_function(&[(0, "nums", list.clone()), (1, "target", named_type("i64"))], list);
        let instance = WasmCallableKey(0);
        let mut input = WasmModuleInput::testing_empty();
        input.functions.insert(instance, function);
        input.instances.push(instance);
        let abi = wasm_export_abi(&input, &instance).expect("结算服务必须按签名冻结 ABI");
        assert_eq!(abi.params, vec![WasmAbiCarrier::Handle, WasmAbiCarrier::I64]);
        assert_eq!(abi.returns, WasmAbiCarrier::Handle);
    }

    #[test]
    fn carrier_mapping_covers_scalars_and_handles() {
        use crate::lowering::backends::wasm::mir::abi_schema::WasmAbiType;
        assert_eq!(abi_carrier_name(&WasmAbiType::I32).expect("i32"), "i32");
        assert_eq!(abi_carrier_name(&WasmAbiType::I64).expect("i64"), "i64");
        assert_eq!(abi_carrier_name(&WasmAbiType::F32).expect("f32"), "f32");
        assert_eq!(abi_carrier_name(&WasmAbiType::F64).expect("f64"), "f64");
        assert_eq!(abi_carrier_name(&WasmAbiType::Bool).expect("bool"), "i32");
        assert_eq!(abi_carrier_name(&WasmAbiType::Char).expect("char"), "i32");
        assert_eq!(abi_carrier_name(&WasmAbiType::Utf8).expect("utf8"), "i32");
        assert_eq!(abi_carrier_name(&WasmAbiType::Unit).expect("unit"), "void");
        for schema in [
            WasmAbiType::Array {
                element: Box::new(WasmAbiType::I64),
                ops: crate::lowering::backends::wasm::mir::abi_schema::WasmAbiArrayOps {
                    length: String::new(),
                    get: String::new(),
                    new: String::new(),
                    set: String::new(),
                },
                container: None,
            },
            WasmAbiType::FixedArray { element: Box::new(WasmAbiType::I64), length: 2 },
            WasmAbiType::Option { inner: Box::new(WasmAbiType::I64), box_contract: None },
            WasmAbiType::Tuple { fields: vec![WasmAbiType::I64] },
            WasmAbiType::NamedAggregate {
                name: "ArrayList".to_string(),
                module_path: String::new(),
                generic_args: vec![WasmAbiType::I64],
                fields: Vec::new(),
                constructor: None,
            },
        ] {
            assert_eq!(abi_carrier_name(&schema).expect("handle"), "handle");
        }
    }
}
