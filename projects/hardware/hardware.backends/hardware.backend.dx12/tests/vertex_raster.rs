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
        data_binary_dxbc::validate_dxbc_bytes(bytes).expect("structurally valid DXBC");
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

fn quad(width: f32, height: f32, color: [f32; 4]) -> Vec<Vertex> {
    vec![
        Vertex { position: [0.0, 0.0], color },
        Vertex { position: [width, 0.0], color },
        Vertex { position: [width, height], color },
        Vertex { position: [0.0, 0.0], color },
        Vertex { position: [width, height], color },
        Vertex { position: [0.0, height], color },
    ]
}

#[test]
fn dx12_vertex_buffer_reads_back_straight_translucent_colour() {
    let Some(mut dx12) = open()
    else {
        return;
    };
    let width = 16u32;
    let height = 16u32;
    let pixels =
        dx12.draw_vertex_buffer(&quad(width as f32, height as f32, [1.0, 0.0, 0.0, 0.5]), width, height).expect("draw translucent quad");

    let pixel = pixel_at(&pixels, width as usize, width as usize / 2, height as usize / 2);
    assert!(pixel[3].abs_diff(128) <= 2, "half coverage keeps alpha near 128, got {pixel:?}");
    assert!(pixel[0] >= 250, "straight red is restored after blending, got {pixel:?}");
    assert_eq!(pixel[1], 0, "green is never written, got {pixel:?}");
    assert_eq!(pixel[2], 0, "blue is never written, got {pixel:?}");
}

#[test]
fn dx12_vertex_buffer_blends_over_existing_colour() {
    let Some(mut dx12) = open()
    else {
        return;
    };
    let width = 16u32;
    let height = 16u32;
    let mut vertices = quad(width as f32, height as f32, [0.0, 1.0, 0.0, 1.0]);
    vertices.extend(quad(width as f32, height as f32, [1.0, 0.0, 0.0, 0.5]));
    let pixels = dx12.draw_vertex_buffer(&vertices, width, height).expect("draw overlapping quads");

    let pixel = pixel_at(&pixels, width as usize, width as usize / 2, height as usize / 2);
    assert!(pixel[0].abs_diff(128) <= 8, "half red over green leans halfway, got {pixel:?}");
    assert!(pixel[1].abs_diff(128) <= 8, "half red over green keeps half green, got {pixel:?}");
    assert_eq!(pixel[3], 255, "the opaque background keeps the pixel opaque, got {pixel:?}");
}

#[test]
fn dx12_vertex_buffer_antialiases_diagonal_edges() {
    let Some(mut dx12) = open()
    else {
        return;
    };
    assert!(dx12.raster_samples() >= 1, "the backend always reports a sample count");
    if dx12.raster_samples() == 1 {
        eprintln!("skip diagonal antialiasing check: this adapter exposes no multisample quality levels");
        return;
    }
    let width = 32u32;
    let height = 32u32;
    let (w, h) = (width as f32, height as f32);
    let color = [1.0f32, 0.0, 0.0, 1.0];
    let vertices = [Vertex { position: [0.0, 0.0], color }, Vertex { position: [w, h], color }, Vertex { position: [0.0, h], color }];
    let pixels = dx12.draw_vertex_buffer(&vertices, width, height).expect("draw diagonal triangle");

    let partial = pixels.as_chunks::<4>().0.iter().filter(|pixel| pixel[3] > 0 && pixel[3] < 255).count();
    assert!(partial > 0, "multisampling must leave partially covered pixels along the diagonal, found none");
}
