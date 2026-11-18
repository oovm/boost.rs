//! crate 级共享的错误投影。
//!
//! 腾讯云与 AI 协议族都要把同一批事实映射成 [`CloudError`]：传输层失败到底是「没发出去」
//! 还是「发出去但不知道结果」，响应体片段怎么压进错误信息。这类判断只写一份，避免两套
//! 口径在同一 crate 里分叉。

use boost_net::NetError;

use crate::tencent::CloudError;

/// 把响应体压成可放进错误信息的片段。
///
/// 只保留前 256 个字符并去掉首尾空白，避免把整页网关 HTML 或超长错误体写进日志。
pub(crate) fn body_snippet(body: &[u8]) -> String {
    const LIMIT: usize = 256;
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    let mut snippet: String = trimmed.chars().take(LIMIT).collect();
    if trimmed.chars().count() > LIMIT {
        snippet.push('…');
    }
    snippet
}

/// 传输层失败 → 分层错误。
///
/// [`NetError::Unsupported`] 是本地构造阶段被拒（非法 URL、非法头、缺少 TLS 能力），
/// 请求确定没有发出；其余错误（拨号失败、超时、读响应中断、连接被关闭）都无法排除请求
/// 已经到达服务端并生效，因此一律归入 [`CloudError::Indeterminate`]。
pub(crate) fn map_transport_error(error: NetError) -> CloudError {
    match error {
        NetError::Unsupported(detail) => CloudError::Transport(detail),
        other => CloudError::Indeterminate { reason: format!("传输层失败：{other}"), request_id: None },
    }
}
