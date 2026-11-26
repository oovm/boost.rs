//! `PE` 属性证书表解码。
//!
//! 安全数据目录（索引 4）指向的是文件偏移而不是 `RVA`。它保存一条或多条八字节
//! 对齐的 `WIN_CERTIFICATE` 条目，承载 Authenticode 载荷。本模块报告条目头以及
//! 载荷是否形如 DER `PKCS#7` 结构；它从不校验签名、证书链或时间戳。

use serde::{Deserialize, Serialize};

use super::{analysis::PeAnalysisError, read::PeImage};

/// 属性证书表在数据目录中的索引。
const DIRECTORY_SECURITY: usize = 4;
/// `WIN_CERTIFICATE` 头：长度、修订与类型。
const CERTIFICATE_HEADER_SIZE: usize = 8;
/// 最多收集的证书条目。
const MAX_CERTIFICATES: usize = 64;

const WIN_CERT_REVISION_1_0: u16 = 0x0100;
const WIN_CERT_REVISION_2_0: u16 = 0x0200;

const WIN_CERT_TYPE_X509: u16 = 0x0001;
const WIN_CERT_TYPE_PKCS_SIGNED_DATA: u16 = 0x0002;
const WIN_CERT_TYPE_RESERVED_1: u16 = 0x0003;
const WIN_CERT_TYPE_TS_STACK_SIGNED: u16 = 0x0004;

/// 一条 `WIN_CERTIFICATE` 条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeCertificate {
    /// 条目头的文件偏移。
    pub offset: u32,
    /// `dwLength`，包含八字节头。
    pub length: u32,
    /// `wRevision`，渲染为 `1.0`、`2.0` 或 `0x<revision>`。
    pub revision: String,
    /// `wCertificateType` 的稳定名称。
    pub certificate_type: String,
    /// 载荷大小，即 `dwLength - 8`。
    pub payload_size: u32,
    /// 载荷是否以 DER `SEQUENCE` 标签开头，即 `PKCS#7` `SignedData` 的常见形态。
    pub pkcs7: bool,
}

/// 属性证书表摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeCertificatesReport {
    /// 安全目录是否非空。
    pub present: bool,
    /// 通过上限的条目，按表序排列。
    pub certificates: Vec<PeCertificate>,
    /// 表声明的条目数。
    pub total: usize,
    /// 安全目录声明大小。
    pub directory_size: u32,
    /// 上限或畸形条目是否截断了列表。
    pub truncated: bool,
}

/// 解码一个 PE 镜像的属性证书表。
pub fn read_certificates(image: &PeImage, bytes: &[u8]) -> Result<PeCertificatesReport, PeAnalysisError> {
    let directory = image.optional.data_directories[DIRECTORY_SECURITY];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(PeCertificatesReport { present: false, certificates: Vec::new(), total: 0, directory_size: 0, truncated: false });
    }

    let mut report =
        PeCertificatesReport { present: true, certificates: Vec::new(), total: 0, directory_size: directory.size, truncated: false };
    // 与其他目录不同，这一目录在 `rva` 字段里存的是文件偏移。
    let base = directory.rva as usize;
    let end = base.saturating_add(directory.size as usize).min(bytes.len());
    if base >= bytes.len() {
        report.truncated = true;
        return Ok(report);
    }

    let mut at = base;
    while at.saturating_add(CERTIFICATE_HEADER_SIZE) <= end {
        let length = read_u32(bytes, at)?;
        if length < CERTIFICATE_HEADER_SIZE as u32 {
            report.truncated = true;
            break;
        }
        let entry_end = at.saturating_add(length as usize);
        if entry_end > end {
            report.truncated = true;
            break;
        }
        let revision = read_u16(bytes, at + 4)?;
        let certificate_type = read_u16(bytes, at + 6)?;

        report.total += 1;
        if report.certificates.len() < MAX_CERTIFICATES {
            let payload = &bytes[at + CERTIFICATE_HEADER_SIZE..entry_end];
            report.certificates.push(PeCertificate {
                offset: at as u32,
                length,
                revision: revision_name(revision),
                certificate_type: certificate_type_name(certificate_type),
                payload_size: length - CERTIFICATE_HEADER_SIZE as u32,
                pkcs7: certificate_type == WIN_CERT_TYPE_PKCS_SIGNED_DATA && payload.first() == Some(&0x30),
            });
        }
        else {
            report.truncated = true;
        }

        // 条目按八字节边界起始。
        at = align_u64(entry_end as u64, 8) as usize;
    }
    if at < end {
        report.truncated = true;
    }
    Ok(report)
}

fn revision_name(revision: u16) -> String {
    match revision {
        WIN_CERT_REVISION_1_0 => "1.0".to_string(),
        WIN_CERT_REVISION_2_0 => "2.0".to_string(),
        other => format!("0x{other:04x}"),
    }
}

fn certificate_type_name(certificate_type: u16) -> String {
    match certificate_type {
        WIN_CERT_TYPE_X509 => "X509".to_string(),
        WIN_CERT_TYPE_PKCS_SIGNED_DATA => "PKCS_SIGNED_DATA".to_string(),
        WIN_CERT_TYPE_RESERVED_1 => "RESERVED_1".to_string(),
        WIN_CERT_TYPE_TS_STACK_SIGNED => "TS_STACK_SIGNED".to_string(),
        other => format!("0x{other:04x}"),
    }
}

fn align_u64(value: u64, alignment: u64) -> u64 {
    value.saturating_add(alignment - 1) & !(alignment - 1)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PeAnalysisError> {
    let raw: [u8; 2] = bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|slice| slice.try_into().ok())
        .ok_or(PeAnalysisError::Truncated { table: "证书表" })?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PeAnalysisError> {
    let raw: [u8; 4] = bytes
        .get(offset..offset.saturating_add(4))
        .and_then(|slice| slice.try_into().ok())
        .ok_or(PeAnalysisError::Truncated { table: "证书表" })?;
    Ok(u32::from_le_bytes(raw))
}
