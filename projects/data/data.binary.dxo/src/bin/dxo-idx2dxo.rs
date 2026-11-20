//! `dxo-idx2dxo` — migrate the four gzip'd MNIST IDX files in a directory
//! into one single-file `.dxo` dataset asset (design contract 43).
//!
//! Usage: `dxo-idx2dxo <input-dir> <output-dir> [name.dxo]`
//!
//! The tool is fully offline: it reads the four `.gz` files, verifies each
//! IDX header against the frozen MNIST geometry, builds the asset
//! deterministically (1024 samples per chunk), publishes it, then re-opens
//! the published files and runs full acceptance before printing the
//! semantic digest.

use std::{collections::BTreeMap, fs, path::Path, process::ExitCode};

use data_binary_dxo::{
    Asset, AssetSpec, DatasetDesc, Dtype, FieldDesc, Kind, Provenance, SplitDesc, TensorInput, build, sample_boundaries, sha256_hex,
};
use data_compression::{CodecLimits, Gzip};

/// Samples stored per chunk.
const SAMPLES_PER_CHUNK: u64 = 1024;

/// Safety cap for one decoded IDX stream (train images ≈ 47 MB).
const MAX_DECODED_BYTES: u64 = 1 << 28;

/// One upstream IDX stream and the tensor it becomes.
struct IdxSource {
    filename: &'static str,
    tensor_key: &'static str,
    magic: u32,
    dims: &'static [u64],
}

const SOURCES: &[IdxSource] = &[
    IdxSource { filename: "train-images-idx3-ubyte.gz", tensor_key: "train/images", magic: 2051, dims: &[60000, 28, 28] },
    IdxSource { filename: "train-labels-idx1-ubyte.gz", tensor_key: "train/labels", magic: 2049, dims: &[60000] },
    IdxSource { filename: "t10k-images-idx3-ubyte.gz", tensor_key: "test/images", magic: 2051, dims: &[10000, 28, 28] },
    IdxSource { filename: "t10k-labels-idx1-ubyte.gz", tensor_key: "test/labels", magic: 2049, dims: &[10000] },
];

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("dxo-idx2dxo: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        return Err("usage: dxo-idx2dxo <input-dir> <output-dir> [name.dxo]".to_string());
    }
    let input_dir = Path::new(&args[1]);
    let output_dir = Path::new(&args[2]);
    let name = args.get(3).map(String::as_str).unwrap_or("mnist.dxo");

    let mut tensors: Vec<TensorInput> = Vec::new();
    let mut boundaries: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut input_sha256: Vec<String> = Vec::new();

    for source in SOURCES {
        let path = input_dir.join(source.filename);
        let gz = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        input_sha256.push(sha256_hex(&gz));

        let limits = CodecLimits::new().with_max_input_bytes(gz.len() as u64).with_max_output_bytes(MAX_DECODED_BYTES);
        let facts = Gzip::decode(&gz, limits).map_err(|e| format!("{}: gzip: {e}", source.filename))?;
        if !facts.complete || facts.trailing_input != 0 {
            return Err(format!("{}: not exactly one complete gzip member", source.filename));
        }

        let payload = parse_idx(&facts.value, source)?;
        let sample_count = source.dims[0];
        let bytes_per_sample = payload.len() as u64 / sample_count;
        boundaries.insert(source.tensor_key.to_string(), sample_boundaries(sample_count, bytes_per_sample, SAMPLES_PER_CHUNK));
        tensors.push(TensorInput { key: source.tensor_key.to_string(), dtype: Dtype::U8, shape: source.dims.to_vec(), data: payload });
    }

    let spec = AssetSpec {
        kind: Kind::Dataset,
        tensors,
        boundaries,
        dataset: Some(DatasetDesc {
            splits: vec![
                SplitDesc {
                    name: "train".to_string(),
                    sample_count: 60000,
                    fields: vec![
                        FieldDesc { name: "image".to_string(), tensor_key: "train/images".to_string(), role: "image".to_string() },
                        FieldDesc { name: "label".to_string(), tensor_key: "train/labels".to_string(), role: "label".to_string() },
                    ],
                },
                SplitDesc {
                    name: "test".to_string(),
                    sample_count: 10000,
                    fields: vec![
                        FieldDesc { name: "image".to_string(), tensor_key: "test/images".to_string(), role: "image".to_string() },
                        FieldDesc { name: "label".to_string(), tensor_key: "test/labels".to_string(), role: "label".to_string() },
                    ],
                },
            ],
        }),
        weights: None,
        provenance: Some(Provenance {
            upstream_uri: Some("hf:ai4waifu/mnist".to_string()),
            // The local copies cannot be tied to a verified upstream
            // revision; the contract keeps unknown evidence as `None`.
            revision: None,
            input_sha256,
            license: Some("unknown".to_string()),
            tool_version: Some(format!("dxo-idx2dxo/{}", env!("CARGO_PKG_VERSION"))),
        }),
        volume_width: 0,
        max_volume_bytes: None,
    };

    let artifact = build(&spec).map_err(|e| format!("build: {e}"))?;
    artifact.publish(output_dir, name).map_err(|e| format!("publish: {e}"))?;

    // Acceptance: re-open from disk and verify every chunk and tensor.
    let main_path = output_dir.join(name);
    let mut asset = Asset::open(&main_path).map_err(|e| format!("open: {e}"))?;
    asset.verify_all().map_err(|e| format!("verify_all: {e}"))?;

    let digest: String = asset.index().semantic_sha256.iter().map(|b| format!("{b:02x}")).collect();
    println!("published {}", main_path.display());
    println!("semantic_sha256 {digest}");
    Ok(())
}

/// Parse one decoded IDX stream and return its payload bytes.
fn parse_idx(raw: &[u8], source: &IdxSource) -> Result<Vec<u8>, String> {
    let ndim = (source.magic & 0xff) as usize;
    let header_len = 4 + 4 * ndim;
    if raw.len() < header_len {
        return Err(format!("{}: truncated IDX header", source.filename));
    }
    let magic = u32::from_be_bytes(raw[0..4].try_into().unwrap());
    if magic != source.magic {
        return Err(format!("{}: IDX magic {magic}, expected {}", source.filename, source.magic));
    }
    let mut dims: Vec<u64> = Vec::new();
    for i in 0..ndim {
        dims.push(u32::from_be_bytes(raw[4 + 4 * i..8 + 4 * i].try_into().unwrap()) as u64);
    }
    if dims != source.dims {
        return Err(format!("{}: IDX dims {dims:?}, expected {:?}", source.filename, source.dims));
    }
    let payload = &raw[header_len..];
    let expected = source.dims.iter().product::<u64>();
    if payload.len() as u64 != expected {
        return Err(format!("{}: payload {} bytes, expected {expected}", source.filename, payload.len()));
    }
    Ok(payload.to_vec())
}
