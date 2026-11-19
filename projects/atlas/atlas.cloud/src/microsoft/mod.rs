//! Microsoft 厂商命名空间。
//!
//! 目前只有 [`readaloud`]：Edge 在线语音合成的非官方接口（B 级准入，见云服务合同）。
//! 一旦接口失效，删除对应模块而不是加兼容分支。

pub mod readaloud;
