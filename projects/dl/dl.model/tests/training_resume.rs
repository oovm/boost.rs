//! 训练、保存、恢复、继续训练的闭环。

use dl_model;
use dl_nn::{Module, mlp, seeded_random};
use dl_tensor::Tensor;
use dl_train::{Sgd, TrainConfig, fit};

fn data() -> (Tensor, Tensor) {
    let inputs = Tensor::from_vec(vec![0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0], [4, 2]).unwrap();
    let targets = Tensor::from_vec(vec![0.0, 2.0, 1.0, 3.0], [4, 1]).unwrap();
    (inputs, targets)
}

#[test]
fn resume_training_from_checkpoint() {
    let (inputs, targets) = data();

    let mut random = seeded_random(11);
    let model = mlp(&[2, 8, 1], &mut random).unwrap();
    let mut optimizer = Sgd::new(model.parameters(), 0.05).with_momentum(0.9);
    let first = fit(&model, &mut optimizer, &inputs, &targets, &TrainConfig::new(300, 4)).unwrap();
    let before_resume = model.predict(&inputs).unwrap();

    let path = std::env::temp_dir().join(format!("boost-dl-resume-{}.safetensors", std::process::id()));
    dl_model::save(&model, &path).unwrap();

    let mut other_random = seeded_random(99);
    let restored = mlp(&[2, 8, 1], &mut other_random).unwrap();
    assert_ne!(restored.predict(&inputs).unwrap().to_vec(), before_resume.to_vec());
    dl_model::load(&restored, &path).unwrap();
    assert_eq!(restored.predict(&inputs).unwrap().to_vec(), before_resume.to_vec());

    let loss_before = first.last().unwrap().mean_loss;
    let mut optimizer = Sgd::new(restored.parameters(), 0.01);
    let second = fit(&restored, &mut optimizer, &inputs, &targets, &TrainConfig::new(100, 4)).unwrap();
    let loss_after = second.last().unwrap().mean_loss;
    assert!(loss_after.is_finite(), "resumed loss is not finite: {loss_after}");
    assert!(loss_after <= loss_before + 1e-3, "resumed training raised the loss: {loss_before} -> {loss_after}");

    std::fs::remove_file(&path).unwrap();
}
