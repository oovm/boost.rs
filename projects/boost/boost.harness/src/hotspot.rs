//! 运行期热点与卡点记录：给产品进程用的常开观测面。
//!
//! 与 [`crate::benchmark`]（离线基准，逐次运行采样）不同，本模块面向产品进程
//! 本身，回答工程师持续关心的问题：运行过程中时间花在哪、卡在哪。热路径上的
//! 埋点调用 [`Hotspot::record`] 把墙钟耗时累计到有界键下；发现明确卡点（锁等待、
//! 磁盘同步、全表扫描、远程超时）时调用 [`Hotspot::stall`] 显式登记一条事件。
//!
//! 机制不能影响产品自身运行，模型按这个前提设计：
//!
//! - 关闭时单次调用只付一次 relaxed 原子读，埋点可以永久留在生产代码里；
//! - 开启时（默认）付一次时钟差值加一次短锁持有，临界区只做累加，不执行调用方
//!   的工作；
//! - 键数与事件数都有界：超过 [`MAX_KEYS`] 的新键并入 [`OVERFLOW_KEY`]，卡点
//!   超过 [`MAX_STALLS`] 挤出最旧的，内存不随运行时长增长；
//! - 每个键同时保留次数、总/最小/最大耗时与 log2 延迟直方图，快照可估计
//!   `p50/p95/p99`，用来区分「少量极慢」与「大量中等慢」。
//!
//! 耗时来自 [`std::time::Instant`]，单调；卡点事件记录距 [`Hotspot`] 创建时刻的
//! 毫秒数，墙钟锚点由宿主自己提供。

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// 热点键上限：超过后新键并入 [`OVERFLOW_KEY`]，保证内存有界。
pub const MAX_KEYS: usize = 256;

/// 溢出键并入的聚合键。
pub const OVERFLOW_KEY: &str = "(overflow)";

/// 卡点环形缓冲容量：超过后挤出最旧事件。
pub const MAX_STALLS: usize = 1024;

/// 直方图桶数：按 `log2(ns)` 分桶，2 的幂各一桶，覆盖 1 ns 到 `u64::MAX` ns。
const BUCKETS: usize = 64;

/// 热点记录器，可跨线程共享。
///
/// 默认开启。所有写入共享一把锁，临界区只做计数与累加；关闭路径不碰锁、不读
/// 时钟。记录器本身不持有任何调用方的工作，drop 或被清空都不影响被测代码。
#[derive(Debug)]
pub struct Hotspot {
    enabled: AtomicBool,
    base: Instant,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    rows: BTreeMap<String, Row>,
    stalls: VecDeque<StallEvent>,
    stall_seq: u64,
    dropped_stalls: u64,
}

#[derive(Debug)]
struct Row {
    hits: u64,
    total_ns: u64,
    min_ns: u64,
    max_ns: u64,
    hist: [u64; BUCKETS],
}

impl Default for Row {
    fn default() -> Self {
        Self { hits: 0, total_ns: 0, min_ns: u64::MAX, max_ns: 0, hist: [0; BUCKETS] }
    }
}

impl Row {
    fn add(&mut self, ns: u64) {
        self.hits = self.hits.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(ns);
        self.min_ns = self.min_ns.min(ns);
        self.max_ns = self.max_ns.max(ns);
        self.hist[bucket(ns)] = self.hist[bucket(ns)].saturating_add(1);
    }

    /// 分位数估计：取累计计数首次达到 `ceil(hits * num / den)` 的桶上界。
    fn percentile(&self, num: u64, den: u64) -> u64 {
        let target = self.hits.saturating_mul(num).saturating_add(den - 1) / den;
        let mut seen = 0u64;
        for (index, &count) in self.hist.iter().enumerate() {
            seen = seen.saturating_add(count);
            if seen >= target.max(1) {
                return bucket_upper(index);
            }
        }
        u64::MAX
    }
}

/// 快照里一个键的聚合行。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct HotspotRow {
    /// 埋点键。
    pub key: String,
    /// 记录次数。
    pub hits: u64,
    /// 总耗时，纳秒。
    pub total_ns: u64,
    /// 最小单次耗时，纳秒。
    pub min_ns: u64,
    /// 最大单次耗时，纳秒。
    pub max_ns: u64,
    /// 平均单次耗时，纳秒。
    pub avg_ns: u64,
    /// `p50` 估计，纳秒（直方图桶上界）。
    pub p50_ns: u64,
    /// `p95` 估计，纳秒（直方图桶上界）。
    pub p95_ns: u64,
    /// `p99` 估计，纳秒（直方图桶上界）。
    pub p99_ns: u64,
}

/// 一条显式登记的卡点事件。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct StallEvent {
    /// 全局序号，1 起始，`reset` 后重新计数。
    pub seq: u64,
    /// 距记录器创建时刻的毫秒数。
    pub at_ms: u64,
    /// 关联的埋点键。
    pub key: String,
    /// 卡点类别，来自调用方有界词表（`fsync`、`lock`、`full-scan`、`timeout`…）。
    pub cause: String,
    /// 人类可读细节。
    pub detail: String,
    /// 关联耗时，纳秒；由 [`Hotspot::stall_after`] 给出。
    pub elapsed_ns: Option<u64>,
}

/// [`Hotspot::snapshot`] 的一致性快照。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct HotspotSnapshot {
    /// 快照时刻记录器是否开启。
    pub enabled: bool,
    /// 距记录器创建时刻的毫秒数。
    pub uptime_ms: u64,
    /// 聚合行，按 `total_ns` 降序、同值按键升序。
    pub rows: Vec<HotspotRow>,
    /// 卡点事件，按序号升序，最多 [`MAX_STALLS`] 条。
    pub stalls: Vec<StallEvent>,
    /// 因环形缓冲溢出而被挤出的事件数。
    pub dropped_stalls: u64,
}

impl Default for Hotspot {
    fn default() -> Self {
        Self::new()
    }
}

impl Hotspot {
    /// 创建一个默认开启的记录器。
    pub fn new() -> Self {
        Self { enabled: AtomicBool::new(true), base: Instant::now(), state: Mutex::new(State::default()) }
    }

    /// 开关记录。关闭后 [`Self::record`]、[`Self::stall`] 与 [`Self::stall_after`]
    /// 只付一次 relaxed 原子读，[`Self::time`] 直接执行闭包、不读时钟。
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// 当前开关状态。
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 在 `key` 下累计一次耗时。
    ///
    /// `key` 必须来自有界词表（操作名、表名），否则超出 [`MAX_KEYS`] 的部分会
    /// 并入 [`OVERFLOW_KEY`]，聚合仍然正确，但失去逐键归因能力。
    pub fn record(&self, key: &str, elapsed: Duration) {
        if !self.is_enabled() {
            return;
        }
        let ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        let mut state = self.lock();
        let known = key == OVERFLOW_KEY || state.rows.contains_key(key);
        let name = if known || state.rows.len() < MAX_KEYS { key } else { OVERFLOW_KEY }.to_owned();
        state.rows.entry(name).or_default().add(ns);
    }

    /// 执行 `body` 并把墙钟耗时记在 `key` 下。关闭时不读时钟。
    pub fn time<T>(&self, key: &str, body: impl FnOnce() -> T) -> T {
        if !self.is_enabled() {
            return body();
        }
        let started = Instant::now();
        let output = body();
        self.record(key, started.elapsed());
        output
    }

    /// 登记一条卡点事件（不含耗时）。
    pub fn stall(&self, key: &str, cause: &str, detail: impl Into<String>) {
        self.push_stall(key, cause, detail.into(), None);
    }

    /// 登记一条带耗时的卡点事件。
    pub fn stall_after(&self, key: &str, cause: &str, detail: impl Into<String>, elapsed: Duration) {
        let ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.push_stall(key, cause, detail.into(), Some(ns));
    }

    fn push_stall(&self, key: &str, cause: &str, detail: String, elapsed_ns: Option<u64>) {
        if !self.is_enabled() {
            return;
        }
        let at_ms = self.base.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let mut state = self.lock();
        state.stall_seq = state.stall_seq.saturating_add(1);
        let seq = state.stall_seq;
        state.stalls.push_back(StallEvent { seq, at_ms, key: key.to_owned(), cause: cause.to_owned(), detail, elapsed_ns });
        if state.stalls.len() > MAX_STALLS {
            state.stalls.pop_front();
            state.dropped_stalls = state.dropped_stalls.saturating_add(1);
        }
    }

    /// 当前聚合结果的一致性快照。
    pub fn snapshot(&self) -> HotspotSnapshot {
        let state = self.lock();
        let mut rows: Vec<HotspotRow> = state
            .rows
            .iter()
            .map(|(key, row)| HotspotRow {
                key: key.clone(),
                hits: row.hits,
                total_ns: row.total_ns,
                min_ns: row.min_ns,
                max_ns: row.max_ns,
                avg_ns: row.total_ns / row.hits.max(1),
                p50_ns: row.percentile(1, 2),
                p95_ns: row.percentile(95, 100),
                p99_ns: row.percentile(99, 100),
            })
            .collect();
        rows.sort_by(|left, right| right.total_ns.cmp(&left.total_ns).then_with(|| left.key.cmp(&right.key)));
        HotspotSnapshot {
            enabled: self.is_enabled(),
            uptime_ms: self.base.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            rows,
            stalls: state.stalls.iter().cloned().collect(),
            dropped_stalls: state.dropped_stalls,
        }
    }

    /// 清空全部记录，记录器可继续使用。开关状态与计时基准保持不变：`reset`
    /// 只清窗口，`uptimeMs` 仍相对进程内创建记录器的时刻。
    pub fn reset(&self) {
        let mut state = self.lock();
        *state = State::default();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `ns` 所在桶：`log2` 下取整，`ns <= 1` 归 0 桶。
fn bucket(ns: u64) -> usize {
    63 - ns.max(1).leading_zeros() as usize
}

/// 桶上界估计值；最高桶返回 `u64::MAX`。
fn bucket_upper(index: usize) -> u64 {
    if index >= BUCKETS - 1 { u64::MAX } else { 1u64 << (index + 1) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_recorder_keeps_nothing() {
        let hotspot = Hotspot::new();
        hotspot.set_enabled(false);
        hotspot.record("db.put", Duration::from_millis(5));
        hotspot.stall("db.publish", "fsync", "disk stalled");
        assert_eq!(hotspot.time("db.query", || 7), 7, "关闭时 time 仍返回闭包结果");
        let snapshot = hotspot.snapshot();
        assert!(!snapshot.enabled);
        assert!(snapshot.rows.is_empty());
        assert!(snapshot.stalls.is_empty());
    }

    #[test]
    fn repeated_records_aggregate_hits_extremes_and_total() {
        let hotspot = Hotspot::new();
        for ms in [1u64, 2, 3] {
            hotspot.record("db.put.write:Note", Duration::from_millis(ms));
        }
        let snapshot = hotspot.snapshot();
        assert_eq!(snapshot.rows.len(), 1);
        let row = &snapshot.rows[0];
        assert_eq!(row.key, "db.put.write:Note");
        assert_eq!(row.hits, 3);
        assert_eq!(row.total_ns, 6_000_000);
        assert_eq!(row.min_ns, 1_000_000);
        assert_eq!(row.max_ns, 3_000_000);
        assert_eq!(row.avg_ns, 2_000_000);
        assert!(row.p50_ns <= row.p95_ns && row.p95_ns <= row.p99_ns, "分位数必须单调: {row:?}");
        assert!(row.p99_ns >= row.max_ns, "p99 上界估计不低于最大值");
    }

    #[test]
    fn rows_rank_by_total_time() {
        let hotspot = Hotspot::new();
        for _ in 0..3 {
            hotspot.record("db.hot", Duration::from_millis(10));
        }
        hotspot.record("db.cold", Duration::from_millis(1));
        let snapshot = hotspot.snapshot();
        assert_eq!(snapshot.rows[0].key, "db.hot", "总耗时最高的键排第一");
    }

    #[test]
    fn keys_beyond_the_cap_fold_into_overflow() {
        let hotspot = Hotspot::new();
        for index in 0..(MAX_KEYS + 8) {
            hotspot.record(&format!("db.key.{index}"), Duration::from_micros(10));
        }
        let snapshot = hotspot.snapshot();
        assert_eq!(snapshot.rows.len(), MAX_KEYS + 1, "溢出键是唯一多出的行");
        let overflow = snapshot.rows.iter().find(|row| row.key == OVERFLOW_KEY).expect("溢出行");
        assert_eq!(overflow.hits, 8);
    }

    #[test]
    fn stalls_are_bounded_and_count_dropped() {
        let hotspot = Hotspot::new();
        for index in 0..(MAX_STALLS + 10) {
            hotspot.stall("db.publish", "fsync", format!("event {index}"));
        }
        let snapshot = hotspot.snapshot();
        assert_eq!(snapshot.stalls.len(), MAX_STALLS);
        assert_eq!(snapshot.dropped_stalls, 10);
        assert_eq!(snapshot.stalls[0].seq, 11, "存活的最旧事件是第 11 条");
        assert_eq!(snapshot.stalls[0].detail, "event 10");
        assert_eq!(snapshot.stalls.last().expect("tail").seq, (MAX_STALLS + 10) as u64);
    }

    #[test]
    fn stall_after_carries_the_measured_duration() {
        let hotspot = Hotspot::new();
        hotspot.stall_after("db.query:Note", "full-scan", "scanned 2000 emitted 3", Duration::from_millis(8));
        let snapshot = hotspot.snapshot();
        assert_eq!(snapshot.stalls[0].elapsed_ns, Some(8_000_000));
        assert_eq!(snapshot.stalls[0].cause, "full-scan");
    }

    #[test]
    fn reset_starts_a_clean_window() {
        let hotspot = Hotspot::new();
        hotspot.record("db.put", Duration::from_millis(1));
        hotspot.stall("db.publish", "fsync", "stalled");
        hotspot.reset();
        let snapshot = hotspot.snapshot();
        assert!(snapshot.rows.is_empty());
        assert!(snapshot.stalls.is_empty());
        assert_eq!(snapshot.dropped_stalls, 0);
        assert!(hotspot.is_enabled(), "reset 不改变开关");
        hotspot.stall("db.publish", "fsync", "again");
        assert_eq!(hotspot.snapshot().stalls[0].seq, 1, "序号从 1 重新开始");
    }

    #[test]
    fn histogram_buckets_cover_the_whole_ns_range() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1), 0);
        assert_eq!(bucket(2), 1);
        assert_eq!(bucket(3), 1);
        assert_eq!(bucket(1_000_000_000), 29, "1e9 落在 [2^29, 2^30) 桶");
        assert_eq!(bucket(u64::MAX), BUCKETS - 1);
        assert_eq!(bucket_upper(0), 2);
        assert_eq!(bucket_upper(BUCKETS - 1), u64::MAX);
    }
}
