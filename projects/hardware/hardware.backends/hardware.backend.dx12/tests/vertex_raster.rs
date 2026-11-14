use hardware_backend_dx12::{Dx12Backend, Vertex};
use hardware_emitter::emit_vertex_raster_dxbc;

fn pixel_at(pixels: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * width + x) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn open() -> Option<Dx12Backend> {
    match Dx12Backend::new() {
        Ok(backend) => Some(backend),
        Err(err) => {
            eprintln!("skip dx12 vertex raster (no usable D3D12 device): {err}");
            None
        }
    }
}

#[test]
fn emitted_vertex_raster_passes_structural_validation() {
    let shaders = emit_vertex_raster_dxbc().expect("DXBC emission");
    for bytes in [&shaders.vs, &shaders.ps] {
        assert_eq!(&bytes[0..4], b"DXBC");
        acorn_dxbc::validate_dxbc_bytes(bytes).expect("structurally valid DXBC");
    }
}

#[test]
fn dx12_vertex_buffer_paints_left_half() {
    let Some(mut dx12) = open()
    else {
        return;
    };
    let width = 32u32;
    let height = 32u32;
    let (w, h) = (width as f32, height as f32);
    let red = [1.0f32, 0.0, 0.0, 1.0];
    let half = w / 2.0;
    let vertices = [
        Vertex { position: [0.0, 0.0], color: red },
        Vertex { position: [half, 0.0], color: red },
        Vertex { position: [half, h], color: red },
        Vertex { position: [0.0, 0.0], color: red },
        Vertex { position: [half, h], color: red },
        Vertex { position: [0.0, h], color: red },
    ];

    let pixels = dx12.draw_vertex_buffer(&vertices, width, height).expect("draw vertex buffer");
    assert_eq!(pixels.len(), (width * height * 4) as usize);

    let stride = width as usize;
    let left = pixel_at(&pixels, stride, stride / 4, height as usize / 2);
    assert_eq!(left, [255, 0, 0, 255], "left half should be opaque red");
    let right = pixel_at(&pixels, stride, 3 * stride / 4, height as usize / 2);
    assert_eq!(right, [0, 0, 0, 0], "right half should stay the transparent clear colour");
}

#[test]
fn dx12_vertex_buffer_interpolates_vertex_colours() {
    let Some(mut dx12) = open()
    else {
        return;
    };
    let width = 16u32;
    let height = 16u32;
    let (w, h) = (width as f32, height as f32);
    let red = [1.0f32, 0.0, 0.0, 1.0];
    let green = [0.0f32, 1.0, 0.0, 1.0];
    let vertices = [
        Vertex { position: [0.0, 0.0], color: red },
        Vertex { position: [w, 0.0], color: green },
        Vertex { position: [w, h], color: green },
        Vertex { position: [0.0, 0.0], color: red },
        Vertex { position: [w, h], color: green },
        Vertex { position: [0.0, h], color: red },
    ];

    let pixels = dx12.draw_vertex_buffer(&vertices, width, height).expect("draw vertex buffer");
    let stride = width as usize;
    let center = pixel_at(&pixels, stride, stride / 2, height as usize / 2);
    assert!(center[0].abs_diff(128) <= 24, "red should be interpolated across the quad, got {center:?}");
    assert!(center[1].abs_diff(128) <= 24, "green should be interpolated across the quad, got {center:?}");
    assert_eq!(center[2], 0, "blue is never written, got {center:?}");
    assert_eq!(center[3], 255, "alpha stays opaque, got {center:?}");

    let left = pixel_at(&pixels, stride, 1, height as usize / 2);
    let right = pixel_at(&pixels, stride, stride - 2, height as usize / 2);
    assert!(left[0] > left[1], "left edge should lean red, got {left:?}");
    assert!(right[1] > right[0], "right edge should lean green, got {right:?}");
}

#[test]
fn dx12_vertex_buffer_rejects_empty_input() {
    let Some(mut dx12) = open()
    else {
        return;
    };
    let err = dx12.draw_vertex_buffer(&[], 8, 8).expect_err("an empty vertex buffer must be rejected");
    assert!(err.to_string().contains("empty"), "unexpected error: {err}");
}
