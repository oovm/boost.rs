//! Contract: build-time WGSL bundles and binding manifests for every
//! compositor operator. Runs without a GPU on every platform.

#![cfg(feature = "wgsl")]

use media_gpu::ShaderBundle;
use media_gpu::{FRAME_ELEMENTS, STOP_ELEMENTS, shader_bundles};

fn bundles() -> Vec<ShaderBundle> {
    shader_bundles().unwrap_or_else(|error| panic!("{error}"))
}

fn size_of(bundle: &ShaderBundle, resource: u32) -> u32 {
    bundle.manifest.min_binding_sizes.iter().find(|size| size.resource == resource).unwrap().min_elements_static
}

#[test]
fn every_operator_emits_a_wgsl_compute_bundle() {
    let bundles = bundles();
    let names: Vec<&str> = bundles.iter().map(|bundle| bundle.name.as_str()).collect();
    assert_eq!(names, ["resample", "vignette", "fill", "v_gradient", "radial"]);
    for bundle in &bundles {
        assert!(bundle.wgsl.contains("@compute"), "{}: {}", bundle.name, bundle.wgsl);
        assert!(bundle.wgsl.contains("@workgroup_size(64)"), "{}", bundle.name);
        assert!(bundle.wgsl.contains("fn composite("), "{}", bundle.name);
        assert_eq!(bundle.manifest.workgroup, [64, 1, 1], "{}", bundle.name);
        assert_eq!(bundle.manifest.abi_version, 2, "{}", bundle.name);
        assert_ne!(bundle.manifest.program_hash, 0, "{}", bundle.name);
        // The kernels need the ABI v2 integer subset: guarded u32 div/mod for
        // the pixel index and saturating f32→u32 for the uniform words.
        for helper in ["ucf_div_u32", "ucf_mod_u32", "ucf_f32_to_u32_trunc"] {
            assert!(bundle.wgsl.contains(helper), "{} misses {helper}", bundle.name);
        }
    }
    // Bit packing and channel extraction spread the shift/round helpers over
    // the variants, so the union covers them.
    let all: String = bundles.iter().map(|bundle| bundle.wgsl.as_str()).collect();
    for helper in ["ucf_shl_u32", "ucf_shr_u32", "ucf_f32_to_u32_round"] {
        assert!(all.contains(helper), "no bundle emits {helper}");
    }
}

#[test]
fn manifests_bind_the_compositor_resource_ids_in_order() {
    for bundle in &bundles() {
        let name = &bundle.name;
        let storage: Vec<u32> = bundle.manifest.bindings.iter().filter(|slot| slot.class == "storage").map(|slot| slot.resource).collect();
        assert_eq!(storage, [0, 1, 2, 3], "{name} resource ids");

        // `hardware_emitter::PARAM_RESOURCE` is the `u32::MAX` sentinel.
        let params = bundle.manifest.bindings.iter().find(|slot| slot.resource == u32::MAX).unwrap();
        assert_eq!(params.class, "uniform", "{name}");
        assert_eq!(params.index, 0, "{name}");
        for (rank, id) in [0u32, 1, 2, 3].into_iter().enumerate() {
            let slot = bundle.manifest.bindings.iter().find(|slot| slot.resource == id).unwrap();
            assert_eq!(slot.index, rank as u32 + 1, "{name} binding {id}");
        }

        // Read/write access matches `GpuCompositor`: src and stops are read,
        // dst and scratch are written.
        assert!(bundle.wgsl.contains("var<storage, read> src"), "{name}");
        assert!(bundle.wgsl.contains("var<storage, read> stops"), "{name}");
        assert!(bundle.wgsl.contains("var<storage, read_write> dst"), "{name}");
        assert!(bundle.wgsl.contains("var<storage, read_write> scratch"), "{name}");

        // The declared element counts travel with the manifest.
        assert_eq!(size_of(bundle, 0), FRAME_ELEMENTS, "{name} src");
        assert_eq!(size_of(bundle, 1), STOP_ELEMENTS, "{name} stops");
        assert_eq!(size_of(bundle, 2), FRAME_ELEMENTS, "{name} dst");
        assert_eq!(size_of(bundle, 3), FRAME_ELEMENTS, "{name} scratch");
    }
}

#[test]
fn params_manifest_starts_with_count() {
    for bundle in &bundles() {
        let offsets = &bundle.manifest.field_offsets;
        assert_eq!(offsets[0].name, "count", "{}", bundle.name);
        assert_eq!(offsets[0].byte_offset, 0, "{}", bundle.name);
        assert!(offsets.iter().any(|field| field.name == "uniform_0"), "{}", bundle.name);
        assert!(offsets.iter().any(|field| field.name == "uniform_11"), "{}", bundle.name);
        // count + 12 floats, padded to a vec4 word group.
        assert_eq!(bundle.manifest.param_bytes, 64, "{}", bundle.name);
        // Offsets are dense and ordered, so a host can pack straight through.
        for (index, field) in offsets.iter().enumerate() {
            assert_eq!(field.byte_offset, index as u32 * 4, "{} {}", bundle.name, field.name);
        }
    }
}

#[test]
fn bundles_are_deterministic() {
    let first = bundles();
    let second = bundles();
    assert_eq!(first, second);
    let hashes: Vec<u64> = first.iter().map(|bundle| bundle.manifest.program_hash).collect();
    let mut unique = hashes.clone();
    unique.dedup();
    assert_eq!(unique.len(), hashes.len(), "every operator has its own program");
}
