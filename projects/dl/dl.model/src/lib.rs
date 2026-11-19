//! 模型状态字典与 checkpoint 读写。
//!
//! 状态字典以“参数名 → 张量”表达；文件容器复用 `data-safetensors`，DL 只负责
//! `F32` 张量与字节之间的映射以及形状校验。加载是严格的：缺少参数或形状不符即报错。

#![warn(missing_docs)]

use std::{collections::BTreeMap, path::Path};

use data_safetensors::{TensorView, decode, encode};
use dl_nn::Module;
use dl_tensor::Tensor;
use dl_types::{DlError, Result};

/// 返回模型的状态字典。
pub fn state_dict(module: &dyn Module) -> BTreeMap<String, Tensor> {
    module.named_parameters().into_iter().map(|(name, parameter)| (name, parameter.value())).collect()
}

/// 保存模型参数到 safetensors 文件。
pub fn save(module: &dyn Module, path: &Path) -> Result<()> {
    let state = state_dict(module);
    let entries: Vec<(String, Vec<usize>, Vec<u8>)> = state
        .iter()
        .map(|(name, tensor)| {
            let bytes = tensor.to_vec().into_iter().flat_map(f32::to_le_bytes).collect::<Vec<u8>>();
            (name.clone(), tensor.shape().to_vec(), bytes)
        })
        .collect();
    let views: BTreeMap<String, TensorView<'_>> =
        entries.iter().map(|(name, shape, bytes)| (name.clone(), TensorView { dtype: "F32", shape, bytes })).collect();
    let encoded = encode(&views).map_err(|error| DlError::Format { detail: error.to_string() })?;
    std::fs::write(path, encoded).map_err(|error| DlError::Format { detail: error.to_string() })
}

/// 从 safetensors 文件加载模型参数。
pub fn load(module: &dyn Module, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).map_err(|error| DlError::Format { detail: error.to_string() })?;
    let decoded = decode(&bytes).map_err(|error| DlError::Format { detail: error.to_string() })?;
    load_state_dict(module, &decoded)
}

/// 把已解码的张量装入模型，校验名称、dtype 与形状。
pub fn load_state_dict(module: &dyn Module, tensors: &BTreeMap<String, data_safetensors::OwnedTensor>) -> Result<()> {
    for (name, parameter) in module.named_parameters() {
        let entry = tensors.get(&name).ok_or_else(|| DlError::Format { detail: format!("checkpoint is missing parameter {name:?}") })?;
        if entry.dtype != "F32" {
            return Err(DlError::Format { detail: format!("parameter {name:?} has dtype {}, expected F32", entry.dtype) });
        }
        if entry.shape.as_slice() != parameter.shape() {
            return Err(DlError::Format {
                detail: format!("parameter {name:?} shape {:?} does not match checkpoint {:?}", parameter.shape(), entry.shape),
            });
        }
        let values = entry.bytes.chunks_exact(4).map(|chunk| f32::from_le_bytes(chunk.try_into().expect("4 bytes"))).collect::<Vec<f32>>();
        parameter.set_values(&values)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_nn::{mlp, seeded_random};
    use dl_types::Result as DlResult;

    fn checkpoint_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_nanos();
        std::env::temp_dir().join(format!("boost-dl-{tag}-{}-{nanos}.safetensors", std::process::id()))
    }

    fn sample() -> Tensor {
        Tensor::from_vec(vec![0.0, 1.0, 1.0, 0.0], [2, 2]).unwrap()
    }

    #[test]
    fn save_and_reload_reproduces_inference() -> DlResult<()> {
        let mut random = seeded_random(23);
        let source = mlp(&[2, 6, 1], &mut random)?;
        let path = checkpoint_path("roundtrip");
        save(&source, &path)?;

        let mut other_random = seeded_random(99);
        let restored = mlp(&[2, 6, 1], &mut other_random)?;
        let before = restored.predict(&sample())?;
        load(&restored, &path)?;
        let after = restored.predict(&sample())?;
        let expected = source.predict(&sample())?;

        assert_eq!(after.to_vec(), expected.to_vec());
        assert_ne!(before.to_vec(), after.to_vec());
        std::fs::remove_file(&path).expect("cleanup checkpoint");
        Ok(())
    }

    #[test]
    fn load_rejects_shape_mismatch() {
        let mut random = seeded_random(5);
        let source = mlp(&[2, 4, 1], &mut random).unwrap();
        let path = checkpoint_path("mismatch");
        save(&source, &path).unwrap();

        let mut other_random = seeded_random(6);
        let target = mlp(&[2, 5, 1], &mut other_random).unwrap();
        assert!(load(&target, &path).is_err());
        std::fs::remove_file(&path).expect("cleanup checkpoint");
    }
}
