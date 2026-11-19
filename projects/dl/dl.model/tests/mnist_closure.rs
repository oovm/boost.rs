//! 真实 MNIST 闭包：训练、checkpoint 重启、推理精度。
//!
//! 需要把 IDX 文件放在 `BOOST_MNIST_DIR` 下；未设置时跳过，避免 Boost 默认测试依赖网络或数据。
//! 下游（DXO）才做全量训练；这里只证明整条闭包在真实数据上闭合。

use std::path::{Path, PathBuf};

use dl_autograd::Var;
use dl_model;
use dl_nn::{Module, mlp, seeded_random};
use dl_tensor::Tensor;
use dl_train::{Adam, TrainConfig, fit};
use ml_data::Dataset;
use ml_metrics::accuracy;

fn dataset_root() -> Option<PathBuf> {
    std::env::var_os("BOOST_MNIST_DIR").map(PathBuf::from)
}

fn read(root: &Path, name: &str) -> Vec<u8> {
    std::fs::read(root.join(name)).unwrap_or_else(|error| panic!("read {name}: {error}"))
}

fn argmax_rows(values: &Tensor, rows: usize, classes: usize) -> Vec<u32> {
    let data = values.to_vec();
    (0..rows)
        .map(|row| {
            let slice = &data[row * classes..(row + 1) * classes];
            let mut best = 0usize;
            for (index, value) in slice.iter().enumerate() {
                if *value > slice[best] {
                    best = index;
                }
            }
            best as u32
        })
        .collect()
}

#[test]
fn mnist_train_resume_and_infer() {
    let Some(root) = dataset_root() else {
        println!("BOOST_MNIST_DIR is not set; skipping the MNIST closure");
        return;
    };
    let train = Dataset::from_idx_limited(
        &read(&root, "train-images-idx3-ubyte"),
        &read(&root, "train-labels-idx1-ubyte"),
        2048,
    )
    .expect("train subset");
    let test = Dataset::from_idx_limited(
        &read(&root, "t10k-images-idx3-ubyte"),
        &read(&root, "t10k-labels-idx1-ubyte"),
        512,
    )
    .expect("test subset");
    assert_eq!(train.feature_count(), 784);
    assert_eq!(train.class_count(), 10);

    let mut random = seeded_random(20261120);
    let model = mlp(&[784, 64, 10], &mut random).expect("mlp");
    let (inputs, targets) = train.tensors().expect("train tensors");
    let mut optimizer = Adam::new(model.parameters(), 0.005);
    let reports = fit(&model, &mut optimizer, &inputs, &targets, &TrainConfig { epochs: 3, batch_size: 64 }).expect("fit");

    let first = reports.first().expect("epochs").mean_loss;
    let last = reports.last().expect("epochs").mean_loss;
    assert!(last.is_finite() && last < first, "MNIST loss must fall: {first} -> {last}");

    let (test_inputs, _) = test.tensors().expect("test tensors");
    let predictions = argmax_rows(&model.forward(&Var::leaf(test_inputs.clone())).expect("inference").value(), test.len(), 10);
    let labels = test.samples().iter().map(|sample| sample.label).collect::<Vec<u32>>();
    let accuracy = accuracy(&predictions, &labels).expect("accuracy");
    assert!(accuracy > 0.6, "one-shot MNIST accuracy too low: {accuracy}");

    let path = std::env::temp_dir().join(format!("boost-mnist-{}.safetensors", std::process::id()));
    let reference = model.forward(&Var::leaf(test_inputs.clone())).expect("inference").value();
    dl_model::save(&model, &path).expect("save checkpoint");
    let mut random = seeded_random(1);
    let restored = mlp(&[784, 64, 10], &mut random).expect("mlp");
    dl_model::load(&restored, &path).expect("load checkpoint");
    let restored_prediction = restored.forward(&Var::leaf(test_inputs)).expect("restored inference").value();
    assert_eq!(restored_prediction.to_vec(), reference.to_vec());
    let _ = std::fs::remove_file(path);
}
