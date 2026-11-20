//! `dxo-build` — generic `.dxo` asset builder (design contract 43).
//!
//! Usage: `dxo-build <build.json> [output-dir]`
//!
//! Reads a `dxo.build.v1` JSON spec, loads each tensor as a byte range of a
//! staged blob file next to the spec, builds the asset deterministically,
//! publishes it into `output-dir` (default: the spec's directory), then
//! re-opens and runs full acceptance before printing the semantic digest.
//!
//! Format knowledge (IDX, tar, parquet, safetensors) stays in the caller
//! (the TS prepare scripts); this binary only knows the container.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::ExitCode,
};

use serde::Deserialize;

use data_binary_dxo::{
    Asset, AssetSpec, DatasetDesc, Dtype, FieldDesc, Kind, Provenance, SplitDesc, TensorInput, WeightsDesc, build, sample_boundaries,
};

const SCHEMA: &str = "dxo.build.v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BuildSpec {
    schema: String,
    kind: KindSpec,
    output: String,
    tensors: Vec<TensorSpec>,
    #[serde(default)]
    dataset: Option<DatasetSpec>,
    #[serde(default)]
    weights: Option<WeightsSpec>,
    #[serde(default)]
    provenance: Option<ProvenanceSpec>,
}

#[derive(Debug, Deserialize)]
enum KindSpec {
    #[serde(rename = "dataset")]
    Dataset,
    #[serde(rename = "weights")]
    Weights,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TensorSpec {
    key: String,
    dtype: String,
    shape: Vec<u64>,
    file: String,
    #[serde(default)]
    offset: u64,
    length: u64,
    /// Explicit absolute logical split points; mutually exclusive with `samplesPerChunk`.
    #[serde(default)]
    boundaries: Option<Vec<u64>>,
    #[serde(default)]
    samples_per_chunk: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DatasetSpec {
    splits: Vec<SplitSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SplitSpec {
    name: String,
    sample_count: u64,
    fields: Vec<FieldSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FieldSpec {
    name: String,
    tensor_key: String,
    role: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WeightsSpec {
    architecture_id: String,
    state_dict_contract: String,
    entries: Vec<WeightEntrySpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WeightEntrySpec {
    tensor_key: String,
    role: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProvenanceSpec {
    #[serde(default)]
    upstream_uri: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    input_sha256: Vec<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    tool_version: Option<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("dxo-build: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        return Err("usage: dxo-build <build.json> [output-dir]".to_string());
    }
    let spec_path = PathBuf::from(&args[1]);
    let base_dir = spec_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let output_dir = args.get(2).map(PathBuf::from).unwrap_or_else(|| base_dir.clone());

    let raw = std::fs::read(&spec_path).map_err(|e| format!("{}: {e}", spec_path.display()))?;
    let spec: BuildSpec = serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", spec_path.display()))?;
    if spec.schema != SCHEMA {
        return Err(format!("unsupported schema {:?}, expected {SCHEMA:?}", spec.schema));
    }

    let mut tensors: Vec<TensorInput> = Vec::new();
    let mut boundaries: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for t in &spec.tensors {
        let dtype = match t.dtype.as_str() {
            "u8" => Dtype::U8,
            "f32" => Dtype::F32,
            other => return Err(format!("tensor {}: unsupported dtype {other:?}", t.key)),
        };
        let elem = if dtype == Dtype::U8 { 1u64 } else { 4 };
        let product: u64 = t.shape.iter().product();
        if product.checked_mul(elem) != Some(t.length) {
            return Err(format!("tensor {}: shape {:?} x {elem} bytes != length {}", t.key, t.shape, t.length));
        }
        let data = read_range(&base_dir.join(&t.file), t.offset, t.length)?;
        if let Some(list) = &t.boundaries {
            if t.samples_per_chunk.is_some() {
                return Err(format!("tensor {}: boundaries and samplesPerChunk are mutually exclusive", t.key));
            }
            boundaries.insert(t.key.clone(), list.clone());
        }
        else if let Some(spc) = t.samples_per_chunk {
            let sample_count = *t.shape.first().ok_or_else(|| format!("tensor {}: empty shape", t.key))?;
            if sample_count == 0 || t.length % sample_count != 0 {
                return Err(format!("tensor {}: length not divisible by sample count", t.key));
            }
            let bytes_per_sample = t.length / sample_count;
            boundaries.insert(t.key.clone(), sample_boundaries(sample_count, bytes_per_sample, spc));
        }
        tensors.push(TensorInput { key: t.key.clone(), dtype, shape: t.shape.clone(), data });
    }

    let kind = match spec.kind {
        KindSpec::Dataset => Kind::Dataset,
        KindSpec::Weights => Kind::Weights,
    };
    let dataset = spec.dataset.map(|d| DatasetDesc {
        splits: d
            .splits
            .into_iter()
            .map(|s| SplitDesc {
                name: s.name,
                sample_count: s.sample_count,
                fields: s.fields.into_iter().map(|f| FieldDesc { name: f.name, tensor_key: f.tensor_key, role: f.role }).collect(),
            })
            .collect(),
    });
    let weights = spec.weights.map(|w| WeightsDesc {
        architecture_id: w.architecture_id,
        state_dict_contract: w.state_dict_contract,
        entries: w.entries.into_iter().map(|e| data_binary_dxo::WeightEntry { tensor_key: e.tensor_key, role: e.role }).collect(),
    });
    let provenance = spec.provenance.map(|p| Provenance {
        upstream_uri: p.upstream_uri,
        revision: p.revision,
        input_sha256: p.input_sha256,
        license: p.license,
        tool_version: p.tool_version,
    });

    let asset_spec = AssetSpec { kind, tensors, boundaries, dataset, weights, provenance, volume_width: 0, max_volume_bytes: None };

    let artifact = build(&asset_spec).map_err(|e| format!("build: {e}"))?;
    artifact.publish(&output_dir, &spec.output).map_err(|e| format!("publish: {e}"))?;

    let main_path = output_dir.join(&spec.output);
    let mut asset = Asset::open(&main_path).map_err(|e| format!("open: {e}"))?;
    asset.verify_all().map_err(|e| format!("verify_all: {e}"))?;

    let digest: String = asset.index().semantic_sha256.iter().map(|b| format!("{b:02x}")).collect();
    println!("published {}", main_path.display());
    println!("semantic_sha256 {digest}");
    Ok(())
}

fn read_range(path: &Path, offset: u64, length: u64) -> Result<Vec<u8>, String> {
    let mut fh = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    fh.seek(SeekFrom::Start(offset)).map_err(|e| format!("{}: seek: {e}", path.display()))?;
    let mut buf = vec![0u8; length as usize];
    fh.read_exact(&mut buf).map_err(|e| format!("{}: read: {e}", path.display()))?;
    Ok(buf)
}
