#![deny(missing_docs)]
//! 动画桥接：把 `animator.core` 的时间轴逐帧取样并序列化为 SVG 帧序列。
//!
//! `animator.core` 只负责“在某一秒是什么场景”，不产出可交付文件。本 crate 补上
//! 交付侧的最后一跳：
//!
//! ```text
//! Compiled -> plan_frames(fps) -> 逐帧 DisplayScene -> to_svg() -> frame-0000.svg ...
//! ```
//!
//! 帧内容完全来自 [`Compiled::sample_frame`] 与 [`plotter_scene::DisplayScene::to_svg`]，本层不插值、
//! 不改几何、不重新排版，也不读时钟。画布尺寸在整条时间轴上必须一致，尺寸变化会
//! 显式失败，避免宿主拿到一套无法直接编码为视频的帧。

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use animator_core::Compiled;

/// 一条时间轴在给定帧率下的帧计划。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramePlan {
    /// 帧率。
    pub fps: u32,
    /// 覆盖整条时间轴所需的整帧数量。
    pub frames: u32,
}

/// 一次帧序列写出的结果，带可复核的确定性指纹。
#[derive(Clone, Debug, PartialEq)]
pub struct BridgeReport {
    /// 帧率。
    pub fps: u32,
    /// 写出的帧数量。
    pub frames: u32,
    /// 时间轴总长（秒）。
    pub seconds: f64,
    /// 画布宽度，来自被取样的场景。
    pub width: f64,
    /// 画布高度，来自被取样的场景。
    pub height: f64,
    /// 所有帧 SVG 字节的 FNV-1a，相同程序与帧率重复写出应完全相同。
    pub fingerprint: String,
}

/// 桥接失败的原因。
#[derive(Debug)]
pub enum BridgeError {
    /// 帧率为 0，无法定义帧时间。
    FpsZero,
    /// 请求的帧号落在帧计划之外。
    FrameOutOfRange {
        /// 请求的帧号。
        frame: u32,
        /// 该帧率下的总帧数。
        frames: u32,
    },
    /// 某帧的画布尺寸与首帧不同，无法作为同一条视频的帧序列。
    SizeChanged {
        /// 尺寸发生变化的首个帧号。
        frame: u32,
    },
    /// 场景校验或序列化失败。
    Scene {
        /// 出问题的帧号。
        frame: u32,
        /// 上游场景错误。
        source: plotter_scene::SceneError,
    },
    /// 目录或文件写入失败。
    Io {
        /// 出问题的帧号，创建目录时的失败记为 0。
        frame: u32,
        /// 出问题的路径。
        path: PathBuf,
        /// 上游 I/O 错误。
        source: io::Error,
    },
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FpsZero => write!(formatter, "帧率不能为 0"),
            Self::FrameOutOfRange { frame, frames } => {
                write!(formatter, "帧号 {frame} 超出该帧率下的 {frames} 帧计划")
            }
            Self::SizeChanged { frame } => write!(formatter, "第 {frame} 帧的画布尺寸与首帧不同"),
            Self::Scene { frame, source } => write!(formatter, "第 {frame} 帧无法序列化为 SVG：{source}"),
            Self::Io { frame, path, source } => {
                write!(formatter, "第 {frame} 帧写文件失败 `{}`：{source}", path.display())
            }
        }
    }
}

impl std::error::Error for BridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scene { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 计算帧率对应的帧计划；`fps == 0` 显式拒绝。
pub fn plan_frames(compiled: &Compiled, fps: u32) -> Result<FramePlan, BridgeError> {
    let frames = compiled.frame_count(fps).ok_or(BridgeError::FpsZero)?;
    Ok(FramePlan { fps, frames })
}

/// 取样单帧并序列化为 SVG。
///
/// 越界帧号显式拒绝，不交给 `sample_frame` 的钳制语义去静默返回末帧。
pub fn frame_svg(compiled: &Compiled, fps: u32, frame: u32) -> Result<String, BridgeError> {
    let plan = plan_frames(compiled, fps)?;
    if frame >= plan.frames {
        return Err(BridgeError::FrameOutOfRange { frame, frames: plan.frames });
    }
    compiled.sample_frame(fps, frame).ok_or(BridgeError::FpsZero)?.to_svg().map_err(|source| BridgeError::Scene { frame, source })
}

/// 把整条时间轴逐帧写出为 `frame-0000.svg` 形式的文件。
///
/// 目录不存在时递归创建。返回的 [`BridgeReport`] 记录帧数、时长、画布尺寸与所有
/// 帧字节的指纹；相同程序和帧率重复写出得到相同文件名、相同内容与相同指纹。
pub fn write_frames(compiled: &Compiled, fps: u32, directory: impl AsRef<Path>) -> Result<BridgeReport, BridgeError> {
    let plan = plan_frames(compiled, fps)?;
    let directory = directory.as_ref();
    fs::create_dir_all(directory)
        .map_err(|source| BridgeError::Io { frame: 0, path: directory.to_path_buf(), source })?;

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut size: Option<(f64, f64)> = None;
    for frame in 0..plan.frames {
        let scene = compiled.sample_frame(fps, frame).ok_or(BridgeError::FpsZero)?;
        let current = (scene.size.width, scene.size.height);
        match size {
            None => size = Some(current),
            Some(expected) if expected != current => return Err(BridgeError::SizeChanged { frame }),
            Some(_) => {}
        }
        let svg = scene.to_svg().map_err(|source| BridgeError::Scene { frame, source })?;
        let path = directory.join(format!("frame-{frame:04}.svg"));
        fs::write(&path, &svg).map_err(|source| BridgeError::Io { frame, path, source })?;
        hash = fnv1a(hash, svg.as_bytes());
    }

    let (width, height) = size.unwrap_or_default();
    Ok(BridgeReport {
        fps: plan.fps,
        frames: plan.frames,
        seconds: compiled.duration_seconds(),
        width,
        height,
        fingerprint: format!("{hash:016x}"),
    })
}

/// 把一段字节续进 FNV-1a 64 状态，与其它产物的指纹口径一致。
fn fnv1a(mut state: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    state
}
