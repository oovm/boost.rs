//! Shared helpers for `.dxo` integration tests.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use data_binary_dxo::{AssetSpec, DatasetDesc, Dtype, FieldDesc, Kind, Provenance, SplitDesc, TensorInput};

/// Create a fresh temp directory for a test tag.
pub fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("dxo-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

pub fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Deterministic pseudo bytes for a tensor payload.
pub fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (seed as usize + i * 37 + (i / 251)) as u8).collect()
}

/// A small MNIST-shaped dataset spec: 10 train + 5 test samples, 4x4 images.
pub fn mnist_like_spec(width: u64, max_volume_bytes: Option<u64>, samples_per_chunk: u64) -> AssetSpec {
    let tensors = vec![
        TensorInput { key: "train/images".into(), dtype: Dtype::U8, shape: vec![10, 4, 4], data: payload(1, 160) },
        TensorInput { key: "train/labels".into(), dtype: Dtype::U8, shape: vec![10], data: payload(2, 10) },
        TensorInput { key: "test/images".into(), dtype: Dtype::U8, shape: vec![5, 4, 4], data: payload(3, 80) },
        TensorInput { key: "test/labels".into(), dtype: Dtype::U8, shape: vec![5], data: payload(4, 5) },
    ];
    let mut boundaries = BTreeMap::new();
    boundaries.insert("train/images".to_string(), data_binary_dxo::sample_boundaries(10, 16, samples_per_chunk));
    boundaries.insert("train/labels".to_string(), data_binary_dxo::sample_boundaries(10, 1, samples_per_chunk));
    boundaries.insert("test/images".to_string(), data_binary_dxo::sample_boundaries(5, 16, samples_per_chunk));
    boundaries.insert("test/labels".to_string(), data_binary_dxo::sample_boundaries(5, 1, samples_per_chunk));
    AssetSpec {
        kind: Kind::Dataset,
        tensors,
        boundaries,
        dataset: Some(DatasetDesc {
            splits: vec![
                SplitDesc {
                    name: "train".into(),
                    sample_count: 10,
                    fields: vec![
                        FieldDesc { name: "images".into(), tensor_key: "train/images".into(), role: "input".into() },
                        FieldDesc { name: "labels".into(), tensor_key: "train/labels".into(), role: "label".into() },
                    ],
                },
                SplitDesc {
                    name: "test".into(),
                    sample_count: 5,
                    fields: vec![
                        FieldDesc { name: "images".into(), tensor_key: "test/images".into(), role: "input".into() },
                        FieldDesc { name: "labels".into(), tensor_key: "test/labels".into(), role: "label".into() },
                    ],
                },
            ],
        }),
        weights: None,
        provenance: Some(Provenance {
            upstream_uri: Some("https://example.invalid/mnist".into()),
            revision: Some("unknown".into()),
            input_sha256: vec!["unknown".into()],
            license: Some("unknown".into()),
            tool_version: Some("test".into()),
        }),
        volume_width: width,
        max_volume_bytes,
    }
}
