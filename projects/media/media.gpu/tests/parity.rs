//! GPU/CPU parity: every compositor operator must reproduce the CPU
//! reference on the same frame. Exact byte match is expected where the math
//! is pure arithmetic; resample and vignette allow ±1 because transcendental
//! helpers (`sin_cos` contraction, `hypot` vs `sqrt`) can land a rounding
//! cast on the other side of a half.

use media_gpu::GpuCompositor;
use media_image::Frame;
use media_post::Vignette;
use media_raster::Paint;

/// A deterministic test frame with structure in all three channels.
fn test_frame(w: u32, h: u32) -> Frame {
    let mut frame = Frame::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let p = ((y * w + x) as usize) * 4;
            frame.pixels[p] = ((x * 7 + y * 13) % 256) as u8;
            frame.pixels[p + 1] = ((x * 29 + y * 3) % 256) as u8;
            frame.pixels[p + 2] = ((x + y * 61) % 256) as u8;
            frame.pixels[p + 3] = 255;
        }
    }
    frame
}

/// Acquire a hardware adapter. A software rasterizer (WARP / Microsoft Basic
/// Render Driver) is skipped explicitly: CI hosts such as `windows-latest`
/// only expose one, and dispatching on it crashes instead of returning an
/// error. Without either adapter the test reports and passes — the CPU
/// reference is the contract, and the host degrades explicitly.
fn compositor() -> Option<GpuCompositor> {
    match GpuCompositor::new() {
        Ok(compositor) => Some(compositor),
        Err(_) => {
            let probe = media_gpu::probe_gpu();
            if probe.device_type.as_deref() == Some("software") {
                eprintln!("SKIP gpu parity: software adapter ({})", probe.adapter_name.as_deref().unwrap_or("unknown"));
                return None;
            }
            match GpuCompositor::with_fallback_adapter() {
                Ok(compositor) => Some(compositor),
                Err(error) => {
                    eprintln!("SKIP gpu parity: no adapter available ({error})");
                    None
                }
            }
        }
    }
}

fn max_diff(a: &[u8], b: &[u8]) -> i32 {
    a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).abs()).max().unwrap_or(0)
}

#[test]
fn fill_matches_the_cpu_reference_exactly() {
    let Some(mut gpu) = compositor() else { return };
    let mut frame = test_frame(64, 64);
    gpu.fill(&mut frame, [12, 200, 77, 255]).expect("gpu fill");
    let mut reference = test_frame(64, 64);
    reference.fill(geometry::Rgba([12, 200, 77, 255]));
    assert_eq!(frame.pixels, reference.pixels, "fill must write the exact bytes");
}

#[test]
fn v_gradient_matches_the_cpu_reference_exactly() {
    let Some(mut gpu) = compositor() else { return };
    let stops = [[8u8, 10, 20, 255], [40, 90, 160, 255], [200, 180, 90, 255]];
    let mut frame = test_frame(64, 48);
    gpu.v_gradient(&mut frame, &stops).expect("gpu v_gradient");
    let mut reference = test_frame(64, 48);
    let colors: Vec<geometry::Rgba> = stops.iter().map(|c| geometry::Rgba(*c)).collect();
    reference.v_gradient(&colors);
    assert_eq!(frame.pixels, reference.pixels, "v_gradient must reproduce Rgba::lerp bit for bit");
}

#[test]
fn radial_matches_the_cpu_reference_within_one_step() {
    let Some(mut gpu) = compositor() else { return };
    let mut frame = test_frame(64, 48);
    gpu.radial(&mut frame, [12, 18, 38, 255], [4, 6, 12, 255]).expect("gpu radial");
    let mut reference = test_frame(64, 48);
    reference.radial(geometry::Rgba([12, 18, 38, 255]), geometry::Rgba([4, 6, 12, 255]));
    // The shader may contract `dx*dx + dy*dy` into an fma, moving the sqrt
    // input by one ulp; that can flip a rounding cast, so the honest bound
    // here is one display step (same class as the vignette tolerance).
    assert!(max_diff(&frame.pixels, &reference.pixels) <= 1, "radial must stay within one display step");
}

#[test]
fn vignette_matches_the_cpu_reference_within_one_step() {
    let Some(mut gpu) = compositor() else { return };
    let mut frame = test_frame(64, 64);
    gpu.vignette(&mut frame, 0.6).expect("gpu vignette");
    let mut reference = test_frame(64, 64);
    Vignette::new(0.6).apply(&mut reference);
    assert!(max_diff(&frame.pixels, &reference.pixels) <= 1, "vignette may differ by at most one display step");
    // Alpha is a passthrough in the reference; the GPU must not touch it.
    assert!(frame.pixels.iter().skip(3).step_by(4).all(|a| *a == 255));
}

#[test]
fn resample_matches_the_cpu_reference_within_one_step() {
    let Some(mut gpu) = compositor() else { return };
    let src = test_frame(64, 64);
    // Push-in (taps 1) and pull-out (taps 2) both exercise the box path.
    for (zoom, focus, rotate) in [(1.7f32, [0.42, 0.58], 0.0f32), (0.8, [0.5, 0.5], 12.0)] {
        let mut frame = Frame::new(64, 64);
        gpu.resample(&mut frame, &src, zoom, focus, rotate).expect("gpu resample");
        let mut reference = Frame::new(64, 64);
        reference.resample_from(&src, zoom, focus, rotate);
        assert!(max_diff(&frame.pixels, &reference.pixels) <= 1, "resample zoom={zoom} rotate={rotate} must stay within one display step");
        assert!(frame.pixels.iter().skip(3).step_by(4).all(|a| *a == 255), "resample writes opaque alpha");
    }
}
