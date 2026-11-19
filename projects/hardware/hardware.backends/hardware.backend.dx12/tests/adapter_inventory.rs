//! 打印 DXGI 适配器清单：确认 NPU/MCDM 是否对 D3D12 可见。

use hardware_backend_dx12::Dx12Backend;

#[test]
fn prints_the_adapter_inventory() {
    let adapters = match Dx12Backend::enumerate_adapters() {
        Ok(adapters) => adapters,
        Err(error) => {
            eprintln!("adapter enumeration failed: {error}");
            return;
        }
    };
    for (index, adapter) in adapters.iter().enumerate() {
        println!(
            "[{index}] {} vendor=0x{:04x} device=0x{:04x} vram={} MiB software={}",
            adapter.name,
            adapter.vendor_id,
            adapter.device_id,
            adapter.dedicated_memory / (1024 * 1024),
            adapter.software
        );
    }
    assert!(!adapters.is_empty(), "DXGI must report at least one adapter");

    match Dx12Backend::enumerate_compute_adapters() {
        Ok(compute) => {
            for (index, adapter) in compute.iter().enumerate() {
                println!(
                    "compute[{index}] {} hardware={} vram={} MiB compute={} graphics={}",
                    adapter.name,
                    adapter.hardware_id,
                    adapter.dedicated_memory / (1024 * 1024),
                    adapter.d3d12_core_compute,
                    adapter.d3d12_graphics
                );
            }
        }
        Err(error) => println!("DXCore enumeration failed: {error}"),
    }
}
