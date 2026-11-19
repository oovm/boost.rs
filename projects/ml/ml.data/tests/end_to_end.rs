//! 端到端：`ml.data` 批次 → `dl.nn` + `dl.train` → `ml.metrics` 评估。
//!
//! 训练与评估走同一套 one-hot 目标；准确率从模型输出逐行取最大值得到，不读取训练标签。

use dl_nn::{Module, mlp, seeded_random};
use dl_train::{Sgd, fit_iter};
use dl_types::DlError;
use ml_data::{Dataset, Sample};
use ml_metrics::{accuracy, mse};

fn linearly_separable() -> Dataset {
    Dataset::from_samples(vec![
        Sample::new(vec![-1.0, -1.0], 0),
        Sample::new(vec![-1.0, 1.0], 0),
        Sample::new(vec![-0.5, 0.8], 0),
        Sample::new(vec![1.0, -1.0], 1),
        Sample::new(vec![1.0, 1.0], 1),
        Sample::new(vec![0.5, 0.2], 1),
    ])
    .expect("dataset")
}

fn argmax(row: &[f32]) -> u32 {
    let mut best = 0usize;
    for (index, value) in row.iter().enumerate() {
        if *value > row[best] {
            best = index;
        }
    }
    best as u32
}

#[test]
fn trains_batches_and_scores_with_metrics() -> Result<(), Box<dyn std::error::Error>> {
    let data = linearly_separable();
    let model = mlp(&[2, 8, 2], &mut seeded_random(20261119))?;
    let mut optimizer = Sgd::new(model.parameters(), 0.1).with_momentum(0.9);
    let reports = fit_iter(&model, &mut optimizer, 600, || {
        data.batches(3)
            .expect("batch size")
            .map(|batch| batch.map(|batch| (batch.inputs, batch.targets)).map_err(|error| DlError::Format { detail: error.to_string() }))
    })?;

    let first_loss = reports.first().expect("epochs").mean_loss;
    let last_loss = reports.last().expect("epochs").mean_loss;
    assert!(last_loss < first_loss * 0.5, "loss did not fall: {first_loss} -> {last_loss}");

    let (inputs, targets) = data.tensors()?;
    let outputs = model.predict(&inputs)?;
    let columns = outputs.shape()[1];
    let predictions: Vec<u32> = outputs.to_vec().chunks(columns).map(argmax).collect();
    let labels: Vec<u32> = data.samples().iter().map(|sample| sample.label).collect();
    assert_eq!(accuracy(&predictions, &labels)?, 1.0, "predictions {predictions:?} vs labels {labels:?}");
    assert!(mse(&outputs.to_vec(), &targets.to_vec())? < 0.1);
    Ok(())
}
