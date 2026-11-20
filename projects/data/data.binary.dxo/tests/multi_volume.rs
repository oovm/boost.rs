//! 跨卷验收：volume_width 2..=8 全宽度构建 → 重新打开 → 全量校验 → 逐张量字节回读。
//!
//! 关键不变量：语义摘要只由逻辑内容决定，与分卷布局无关；跨卷张量能完整回读；
//! 每个分卷文件的字节数与索引声明一致。

use std::collections::BTreeMap;
use std::path::PathBuf;

use data_binary_dxo::{build, Asset, AssetSpec, DatasetDesc, Dtype, FieldDesc, Kind, SplitDesc, TensorInput};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dxo-multi-volume-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

type Fixture = (AssetSpec, Vec<(&'static str, Vec<u8>)>);

fn fixture(volume_width: u64, max_volume_bytes: Option<u64>) -> Fixture {
    let images: Vec<u8> = (0..8 * 1024).map(|i| (i % 251) as u8).collect();
    let labels: Vec<u8> = (0..8).map(|i| (i * 7) as u8).collect();
    let logits: Vec<u8> = (0..8 * 2 * 4).map(|i| (i % 97) as u8).collect();

    // 每 1 KiB 一刀，保证 chunk 分散到多个分卷。
    let mut boundaries = BTreeMap::new();
    boundaries.insert("images".to_string(), (1..8).map(|i| i as u64 * 1024).collect::<Vec<u64>>());

    let spec = AssetSpec {
        kind: Kind::Dataset,
        tensors: vec![
            TensorInput { key: "images".into(), dtype: Dtype::U8, shape: vec![8, 1024], data: images.clone() },
            TensorInput { key: "labels".into(), dtype: Dtype::U8, shape: vec![8, 1], data: labels.clone() },
            TensorInput { key: "logits".into(), dtype: Dtype::F32, shape: vec![8, 2], data: logits.clone() },
        ],
        boundaries,
        dataset: Some(DatasetDesc {
            splits: vec![SplitDesc {
                name: "train".into(),
                sample_count: 8,
                fields: vec![
                    FieldDesc { name: "image".into(), tensor_key: "images".into(), role: "feature".into() },
                    FieldDesc { name: "label".into(), tensor_key: "labels".into(), role: "label".into() },
                ],
            }],
        }),
        weights: None,
        provenance: None,
        volume_width,
        max_volume_bytes,
    };
    (spec, vec![("images", images), ("labels", labels), ("logits", logits)])
}

#[test]
fn multi_volume_widths_2_to_8_roundtrip() {
    let (single, _) = fixture(0, None);
    let single_digest = build(&single).expect("single volume build").index.semantic_sha256;

    for width in 2..=8u64 {
        let (spec, expected) = fixture(width, Some(3072));
        let dir = scratch(&format!("w{width}"));
        let artifact = build(&spec).unwrap_or_else(|e| panic!("width {width}: build failed: {e}"));
        assert_eq!(
            artifact.index.semantic_sha256, single_digest,
            "width {width}: semantic digest must not depend on volume layout"
        );
        artifact.publish(&dir, "sample.dxo").unwrap_or_else(|e| panic!("width {width}: publish failed: {e}"));

        let mut asset = Asset::open(dir.join("sample.dxo")).unwrap_or_else(|e| panic!("width {width}: open failed: {e}"));
        assert_eq!(asset.index().volume_width, width, "width {width}: index volume_width");
        assert!(
            asset.index().volumes.len() >= 2,
            "width {width}: expected multiple volumes, got {}",
            asset.index().volumes.len()
        );
        for volume in &asset.index().volumes {
            let path = asset.volume_path(volume.volume_index);
            assert!(path.exists(), "width {width}: missing volume {}", path.display());
            assert_eq!(
                std::fs::metadata(&path).expect("volume metadata").len(),
                volume.file_bytes,
                "width {width}: volume {} size mismatch",
                volume.volume_index
            );
        }

        asset.verify_all().unwrap_or_else(|e| panic!("width {width}: verify_all failed: {e}"));
        for (key, want) in &expected {
            let got = asset.read_tensor(key).unwrap_or_else(|e| panic!("width {width}: read {key} failed: {e}"));
            assert_eq!(&got, want, "width {width}: tensor {key} mismatch");
        }

        let slice = asset.read_samples("train", "image", 3, 2).unwrap_or_else(|e| panic!("width {width}: read_samples failed: {e}"));
        assert_eq!(slice, expected[0].1[3 * 1024..5 * 1024].to_vec(), "width {width}: cross-volume sample slice");
    }
}

#[test]
fn volume_width_out_of_range_rejected() {
    for width in [1u64, 9] {
        let (spec, _) = fixture(width, Some(3072));
        let err = build(&spec).expect_err("width must be rejected");
        assert!(format!("{err}").contains("volume_width"), "width {width}: unexpected error {err}");
    }
    let (spec, _) = fixture(2, None);
    let err = build(&spec).expect_err("missing max_volume_bytes must be rejected");
    assert!(format!("{err}").contains("max_volume_bytes"), "unexpected error {err}");
}
