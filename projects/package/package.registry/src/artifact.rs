use std::path::{Component, Path, PathBuf};

use data_binary_tar::{TarEntryKind, TarGz};
use data_compression::CodecLimits;
use data_encoding::representation::Base64;
use sha2::{Digest, Sha256, Sha512};

use crate::RegistryError;

/// 解压上限：防止不可信制品把磁盘写满。
const MAX_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024;

/// 校验 SRI 完整性（`sha512-…` / `sha256-…`）。
pub fn verify_sri(data: &[u8], integrity: Option<&str>) -> Result<(), RegistryError> {
    let Some(integrity) = integrity.filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let (algo, expected) = integrity.split_once('-').ok_or_else(|| RegistryError::message(format!("invalid integrity format: {integrity}")))?;
    let digest = match algo {
        "sha256" => Sha256::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        other => return Err(RegistryError::message(format!("unsupported integrity algorithm: {other}"))),
    };
    let actual = Base64::standard_strict().encode_text(digest);
    if actual != expected {
        return Err(RegistryError::message(format!("integrity mismatch: expected {expected}, got {actual}")));
    }
    Ok(())
}

/// SHA-256 摘要，形如 `sha256-<hex>`。
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256-{hex}")
}

/// 解出 gzip 压缩的 tarball，去掉可选的 `package/` 前缀。
///
/// 成员路径必须落在 `target_directory` 之内：绝对路径、盘符前缀与 `..` 一律拒绝。
pub fn extract_tarball(data: &[u8], target_directory: &Path) -> Result<(), RegistryError> {
    let limits = CodecLimits::new().with_max_output_bytes(MAX_EXTRACTED_BYTES);
    let archive = TarGz::decode("registry-tarball", data, limits).map_err(|error| RegistryError::message(error.to_string()))?;
    std::fs::create_dir_all(target_directory)?;

    for entry in &archive.archive().entries {
        let Some(relative) = relative_member_path(&entry.path)?
        else {
            continue;
        };
        let destination = target_directory.join(&relative);
        match entry.kind {
            TarEntryKind::Directory => {
                std::fs::create_dir_all(&destination)?;
            }
            TarEntryKind::Symlink | TarEntryKind::HardLink => {
                return Err(RegistryError::message(format!(
                    "tarball member `{}` is a link, which package artifacts must not contain",
                    relative.display()
                )));
            }
            _ if !entry.kind.has_data() => {}
            _ => {
                let payload = archive
                    .payload(entry)
                    .ok_or_else(|| RegistryError::message(format!("tarball member `{}` has no payload", relative.display())))?;
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&destination, payload)?;
            }
        }
    }
    Ok(())
}

/// 计算成员相对路径；空路径与根条目返回 `None`。
fn relative_member_path(raw: &[u8]) -> Result<Option<PathBuf>, RegistryError> {
    let text = String::from_utf8_lossy(raw);
    let mut relative = text.as_ref();
    if let Some(stripped) = relative.strip_prefix("./") {
        relative = stripped;
    }
    if let Some(stripped) = relative.strip_prefix("package/") {
        relative = stripped;
    }
    let relative = relative.trim_end_matches('/');
    if relative.is_empty() || relative == "." {
        return Ok(None);
    }

    let path = Path::new(relative);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RegistryError::message(format!("tarball member `{relative}` escapes the target directory")));
            }
        }
    }
    Ok(Some(path.to_path_buf()))
}
