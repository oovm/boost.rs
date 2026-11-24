use compiler_target::{TargetBackendFamily, TargetLane};

use crate::{ArtifactPartition, BundledBackendCapabilityDescriptor, DriverCompileReport, bundled_backend_capability_descriptor};

pub(crate) fn backend_family_for_partition(partition: &ArtifactPartition) -> TargetBackendFamily {
    match partition.lane {
        TargetLane::Clr => TargetBackendFamily::Clr,
        TargetLane::Jvm => TargetBackendFamily::Jvm,
        TargetLane::Wasm => TargetBackendFamily::Wasm,
        TargetLane::MachineCode => TargetBackendFamily::MachineCode,
        TargetLane::Vm => TargetBackendFamily::Ovm,
        TargetLane::Gpu => TargetBackendFamily::Gpu,
    }
}

pub(crate) fn backend_descriptor_for_partition(partition: &ArtifactPartition) -> Option<BundledBackendCapabilityDescriptor> {
    bundled_backend_capability_descriptor(backend_family_for_partition(partition))
}

pub(crate) fn partition_artifact_name(base_name: &str, partition: &ArtifactPartition, partition_count: usize) -> String {
    if partition_count <= 1 {
        return base_name.to_string();
    }

    let suffix = partition
        .name
        .rsplit("::")
        .next()
        .unwrap_or(partition.name.as_str())
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' })
        .collect::<String>();
    format!("{base_name}__{suffix}")
}

pub(crate) fn merge_partition_reports(
    reports: Vec<(String, DriverCompileReport)>,
    primary_partition_name: Option<&str>,
) -> crate::Result<DriverCompileReport> {
    let mut merged = DriverCompileReport::default();
    let mut primary_contracts = Vec::new();
    let mut secondary_contracts = Vec::new();
    let mut managed_abi: Vec<(String, crate::ManagedAbiContract)> = Vec::new();
    let mut ovm_abi: Vec<(String, crate::OvmAbiSidecar)> = Vec::new();
    let mut export_owners: Vec<(String, String)> = Vec::new();
    for (partition_name, report) in reports {
        merged.artifacts.artifacts.extend(report.artifacts.artifacts);
        if merged.entry_symbol.is_none() {
            merged.entry_symbol = report.entry_symbol.clone();
        }
        // 每个物理产物各持一份 managed ABI 合同；不同分区各自拥有产物是合法的，
        // 只有两个分区声称同一 `(lane, artifact)` 时才是不可合并的歧义。
        for contract in report.managed_abi {
            if let Some((existing_partition, existing)) =
                managed_abi.iter().find(|(_, existing)| existing.lane == contract.lane && existing.artifact == contract.artifact)
            {
                return Err(format!(
                    "多个分区同时声称同一物理产物 `{}` 的 managed ABI 合同（lane `{}`）：`{existing_partition}` 与 `{partition_name}`；拒绝合并歧义合同",
                    existing.artifact, existing.lane
                ));
            }
            managed_abi.push((partition_name.clone(), contract));
        }
        // 每个物理产物各持一份 ABI sidecar；不同分区各自拥有产物是合法的，只有
        // 两个分区声称同一物理产物时才是不可合并的歧义（产物归属/哈希无法唯一）。
        for sidecar in report.ovm_abi {
            if let Some((existing_partition, existing)) = ovm_abi.iter().find(|(_, existing)| existing.artifact == sidecar.artifact) {
                return Err(format!(
                    "多个分区同时声称同一物理产物 `{}` 的 OVM ABI sidecar：`{existing_partition}` 与 `{partition_name}`；拒绝合并歧义 ABI 合同",
                    existing.artifact
                ));
            }
            ovm_abi.push((partition_name.clone(), sidecar));
        }
        if primary_partition_name == Some(partition_name.as_str()) {
            primary_contracts.extend(report.run_contracts);
        }
        else {
            secondary_contracts.extend(report.run_contracts);
        }
        // 导出清单是宿主按逻辑名查找的扁平索引；同一逻辑名被两个分区同时声称时，
        // 宿主无法确定绑定哪一份产物，必须在合并期失败而不是写出歧义索引。
        for export in report.library_exports {
            if let Some((existing_partition, _)) = export_owners.iter().find(|(name, _)| *name == export.logical_name) {
                return Err(format!(
                    "多个分区同时导出同一逻辑名 `{}`：`{existing_partition}` 与 `{partition_name}`；拒绝合并歧义导出索引",
                    export.logical_name
                ));
            }
            export_owners.push((export.logical_name.clone(), partition_name.clone()));
            merged.library_exports.push(export);
        }
    }
    merged.run_contracts = primary_contracts;
    merged.run_contracts.extend(secondary_contracts);
    merged.managed_abi = managed_abi.into_iter().map(|(_, contract)| contract).collect();
    merged.ovm_abi = ovm_abi.into_iter().map(|(_, sidecar)| sidecar).collect();
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManagedAbiContract;

    fn report_with_managed_abi(lane: &str, artifact: &str) -> DriverCompileReport {
        DriverCompileReport { managed_abi: vec![ManagedAbiContract::new(lane, artifact, Vec::new())], ..DriverCompileReport::default() }
    }

    /// 不同物理产物各自拥有 managed ABI；合并不得丢弃第二份。
    #[test]
    fn merges_one_managed_abi_per_physical_artifact() {
        let merged = merge_partition_reports(
            vec![
                ("client".to_string(), report_with_managed_abi("jvm", "client.jar")),
                ("server".to_string(), report_with_managed_abi("clr", "server.dll")),
            ],
            None,
        )
        .expect("不同产物必须可合并");
        assert_eq!(merged.managed_abi.len(), 2);
        assert_eq!(merged.managed_abi.iter().map(|contract| contract.artifact.as_str()).collect::<Vec<_>>(), vec!["client.jar", "server.dll"]);
    }

    /// 两个分区声称同一 `(lane, artifact)` 是归属歧义，必须失败。
    #[test]
    fn rejects_two_partitions_claiming_the_same_managed_artifact() {
        let error = merge_partition_reports(
            vec![
                ("left".to_string(), report_with_managed_abi("jvm", "app.jar")),
                ("right".to_string(), report_with_managed_abi("jvm", "app.jar")),
            ],
            None,
        )
        .expect_err("同一产物被两个分区声称必须失败");
        assert!(error.contains("managed ABI"), "{error}");
        assert!(error.contains("app.jar"), "{error}");
    }

    fn report_with_export(export_name: &str) -> DriverCompileReport {
        DriverCompileReport {
            library_exports: vec![crate::DriverLibraryExport {
                logical_name: export_name.to_string(),
                source_name: export_name.to_string(),
                physical_artifact: format!("{export_name}.dll"),
                invocation: "LoadLibraryW + GetProcAddress".to_string(),
                abi_digest: String::new(),
                abi: "c-abi".to_string(),
                parameters: Some(Vec::new()),
                parameter_names: Some(Vec::new()),
                returns: Some("unit".to_string()),
            }],
            ..DriverCompileReport::default()
        }
    }

    /// 不同分区导出不同逻辑名时必须全部保留。
    #[test]
    fn merges_distinct_library_exports() {
        let merged =
            merge_partition_reports(vec![("a".to_string(), report_with_export("alpha")), ("b".to_string(), report_with_export("beta"))], None)
                .expect("不同逻辑名必须可合并");
        assert_eq!(
            merged.library_exports.iter().map(|export| export.logical_name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }

    /// 同一逻辑名被两个分区同时导出时，宿主无法确定绑定哪份产物，必须在合并期失败。
    #[test]
    fn rejects_duplicate_logical_export_names_across_partitions() {
        let error = merge_partition_reports(
            vec![("left".to_string(), report_with_export("shared")), ("right".to_string(), report_with_export("shared"))],
            None,
        )
        .expect_err("重复逻辑名必须失败");
        assert!(error.contains("歧义导出索引"), "{error}");
        assert!(error.contains("`shared`"), "{error}");
    }
}
