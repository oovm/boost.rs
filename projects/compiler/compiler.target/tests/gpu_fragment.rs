use compiler_target::{
    abstractions::{BackendInputKind, BinaryTarget},
    backends::{
        CompilationOptions,
        gpu::{GpuDxilBackend, GpuSpirvBackend},
    },
    packaging::TargetLane,
};

#[test]
fn gpu_backend_placeholders_accept_spirv_and_dxil() {
    assert!(GpuSpirvBackend::accept(BackendInputKind::SpirvModule, TargetLane::Gpu));
    assert!(GpuDxilBackend::accept(BackendInputKind::DxilContainer, TargetLane::Gpu));
    assert!(!GpuSpirvBackend::accept(BackendInputKind::DxilContainer, TargetLane::Gpu));
}

/// 占位后端不得把「没有实现」伪装成「编译成功且无产物」。
#[test]
fn gpu_placeholders_do_not_claim_compile_success() {
    let options =
        CompilationOptions { target: BinaryTarget::clr(), artifact_name: "gpu_probe".to_string(), emit_debug_symbols: false, optimize: false };
    let spirv = GpuSpirvBackend.compile_placeholder(&options).expect_err("SPIR-V placeholder 必须失败闭合");
    assert_eq!(spirv.backend.as_deref(), Some(GpuSpirvBackend::NAME), "错误必须携带后端归属: {spirv}");
    assert!(spirv.to_string().contains("不得宣称编译成功"), "{spirv}");
    let dxil = GpuDxilBackend.compile_placeholder(&options).expect_err("DXIL placeholder 必须失败闭合");
    assert_eq!(dxil.backend.as_deref(), Some(GpuDxilBackend::NAME), "错误必须携带后端归属: {dxil}");
    assert!(dxil.to_string().contains("不得宣称编译成功"), "{dxil}");
}
