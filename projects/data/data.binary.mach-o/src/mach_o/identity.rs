//! `Mach-O` 镜像身份事实：`LC_UUID`、`LC_BUILD_VERSION` 与 `LC_VERSION_MIN_*`。

/// `LC_BUILD_VERSION` 的平台标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachPlatform {
    /// macOS。
    MacOs,
    /// iOS。
    IOs,
    /// tvOS。
    TvOs,
    /// watchOS。
    WatchOs,
    /// bridgeOS。
    BridgeOs,
    /// Mac Catalyst。
    MacCatalyst,
    /// iOS 模拟器。
    IOsSimulator,
    /// tvOS 模拟器。
    TvOsSimulator,
    /// watchOS 模拟器。
    WatchOsSimulator,
    /// DriverKit。
    DriverKit,
    /// visionOS。
    VisionOs,
    /// visionOS 模拟器。
    VisionOsSimulator,
    /// 未识别的平台值。
    Other(u32),
}

impl MachPlatform {
    /// 从原始平台值映射。
    pub fn from_value(value: u32) -> Self {
        match value {
            1 => Self::MacOs,
            2 => Self::IOs,
            3 => Self::TvOs,
            4 => Self::WatchOs,
            5 => Self::BridgeOs,
            6 => Self::MacCatalyst,
            7 => Self::IOsSimulator,
            8 => Self::TvOsSimulator,
            9 => Self::WatchOsSimulator,
            10 => Self::DriverKit,
            11 => Self::VisionOs,
            12 => Self::VisionOsSimulator,
            other => Self::Other(other),
        }
    }

    /// 报告使用的稳定小写名；未知值统一为 `unknown`。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MacOs => "macos",
            Self::IOs => "ios",
            Self::TvOs => "tvos",
            Self::WatchOs => "watchos",
            Self::BridgeOs => "bridgeos",
            Self::MacCatalyst => "mac-catalyst",
            Self::IOsSimulator => "ios-simulator",
            Self::TvOsSimulator => "tvos-simulator",
            Self::WatchOsSimulator => "watchos-simulator",
            Self::DriverKit => "driverkit",
            Self::VisionOs => "visionos",
            Self::VisionOsSimulator => "visionos-simulator",
            Self::Other(_) => "unknown",
        }
    }
}

/// 平台信息的来源命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachBuildSource {
    /// `LC_BUILD_VERSION`。
    BuildVersion,
    /// `LC_VERSION_MIN_*`。
    VersionMin,
}

/// `LC_BUILD_VERSION` 或 `LC_VERSION_MIN_*` 给出的平台与版本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachBuildInfo {
    /// 目标平台。
    pub platform: MachPlatform,
    /// `minos` / `version`，按 `x.y.z` 打包。
    pub min_os: u32,
    /// `sdk`，按 `x.y.z` 打包。
    pub sdk: u32,
    /// 来源命令。
    pub source: MachBuildSource,
}

impl MachBuildInfo {
    /// 最低系统版本，渲染为 `major.minor.patch`。
    pub fn min_os_version(&self) -> String {
        render_version(self.min_os)
    }

    /// SDK 版本，渲染为 `major.minor.patch`。
    pub fn sdk_version(&self) -> String {
        render_version(self.sdk)
    }
}

/// 把打包版本（如 `0x000D0100`）渲染为 `13.1.0`。
pub fn render_version(value: u32) -> String {
    format!("{}.{}.{}", value >> 16, (value >> 8) & 0xff, value & 0xff)
}

/// 把 16 字节 `LC_UUID` 载荷渲染为规范 UUID 文本。
pub fn uuid_string(bytes: &[u8; 16]) -> String {
    let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}
