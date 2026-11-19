//! 虚拟显存的分段流式加载。
//!
//! 逻辑模型大小不受设备容量限制：段通常常驻磁盘/主机，提交前才换入设备。提交时只要求
//! 当前段装得下；设备预算不足时按 LRU 驱逐到主机，主机也不足时再下沉到磁盘。所有搬运、
//! 驱逐和命中都进入 [`LoadPlan`] 与 [`StreamStats`]，预算靠的是真实字节账，不靠布尔值。

use std::collections::BTreeMap;

/// 内存层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// 加速器可寻址内存。
    Device,
    /// 主机内存。
    Host,
    /// 磁盘后备。
    Disk,
}

/// 段标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentId(pub u64);

/// 一次搬运。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    /// 被搬运的段。
    pub segment: SegmentId,
    /// 来源层级。
    pub from: Tier,
    /// 目标层级。
    pub to: Tier,
    /// 字节数。
    pub bytes: u64,
}

/// 单次提交 / 预取产生的加载计划。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadPlan {
    /// 目标段提交时已驻留设备。
    pub hit: bool,
    /// 实际发生的搬运（含驱逐）。
    pub transfers: Vec<Transfer>,
    /// 被驱逐出设备的段。
    pub evictions: Vec<SegmentId>,
}

/// 累积统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// 提交时直接命中设备的次数。
    pub device_hits: u64,
    /// 从主机换入的次数。
    pub host_loads: u64,
    /// 从磁盘换入的次数。
    pub disk_loads: u64,
    /// 驱逐出设备的次数。
    pub evictions: u64,
    /// 预取成功换入的段数。
    pub prefetched_loads: u64,
    /// 全部搬运字节（含驱逐）。
    pub transferred_bytes: u64,
}

/// 流式加载错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// 未注册的段。
    UnknownSegment(SegmentId),
    /// 重复注册。
    DuplicateSegment(SegmentId),
    /// 单段超过设备容量，任何调度都无法装下。
    DeviceBudgetTooSmall {
        /// 无法装入的段。
        segment: SegmentId,
        /// 段大小。
        bytes: u64,
        /// 设备容量。
        budget: u64,
    },
    /// 主机后备不足。
    HostExhausted {
        /// 需要的字节。
        required: u64,
        /// 可用字节。
        available: u64,
    },
    /// 磁盘后备不足。
    DiskExhausted {
        /// 需要的字节。
        required: u64,
        /// 可用字节。
        available: u64,
    },
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::UnknownSegment(id) => write!(f, "unknown segment {}", id.0),
            StreamError::DuplicateSegment(id) => write!(f, "segment {} registered twice", id.0),
            StreamError::DeviceBudgetTooSmall { segment, bytes, budget } => {
                write!(f, "segment {} needs {bytes} bytes but device budget is {budget}", segment.0)
            }
            StreamError::HostExhausted { required, available } => {
                write!(f, "host backing exhausted: need {required} bytes, have {available}")
            }
            StreamError::DiskExhausted { required, available } => {
                write!(f, "disk backing exhausted: need {required} bytes, have {available}")
            }
        }
    }
}

impl std::error::Error for StreamError {}

/// 虚拟显存分段调度器：设备 / 主机 / 磁盘三层，LRU 驱逐。
pub struct SegmentStreamer {
    device_capacity: u64,
    host_capacity: u64,
    disk_capacity: u64,
    bytes: BTreeMap<SegmentId, u64>,
    tiers: BTreeMap<SegmentId, Tier>,
    device_used: u64,
    host_used: u64,
    disk_used: u64,
    clock: u64,
    last_used: BTreeMap<SegmentId, u64>,
    stats: StreamStats,
}

impl SegmentStreamer {
    /// 以三层容量构造；段默认注册在磁盘。
    pub fn new(device_capacity: u64, host_capacity: u64, disk_capacity: u64) -> Self {
        Self {
            device_capacity,
            host_capacity,
            disk_capacity,
            bytes: BTreeMap::new(),
            tiers: BTreeMap::new(),
            device_used: 0,
            host_used: 0,
            disk_used: 0,
            clock: 0,
            last_used: BTreeMap::new(),
            stats: StreamStats::default(),
        }
    }

    /// 注册一个后备段，初始位于磁盘。
    pub fn register(&mut self, id: SegmentId, bytes: u64) -> Result<(), StreamError> {
        if self.bytes.contains_key(&id) {
            return Err(StreamError::DuplicateSegment(id));
        }
        let available = self.disk_capacity.saturating_sub(self.disk_used);
        if bytes > available {
            return Err(StreamError::DiskExhausted { required: bytes, available });
        }
        self.bytes.insert(id, bytes);
        self.tiers.insert(id, Tier::Disk);
        self.disk_used = self.disk_used.saturating_add(bytes);
        Ok(())
    }

    /// 提交一个段：命中直接返回，否则驱逐 LRU 设备段并换入。
    pub fn submit(&mut self, id: SegmentId) -> Result<LoadPlan, StreamError> {
        let bytes = self.bytes_of(id)?;
        if self.tier_of(id) == Tier::Device {
            self.touch(id);
            self.stats.device_hits += 1;
            return Ok(LoadPlan { hit: true, transfers: Vec::new(), evictions: Vec::new() });
        }
        if bytes > self.device_capacity {
            return Err(StreamError::DeviceBudgetTooSmall { segment: id, bytes, budget: self.device_capacity });
        }

        let mut plan = LoadPlan::default();
        while self.device_used + bytes > self.device_capacity {
            let victim = match self.lru_in(Tier::Device) {
                Some(victim) => victim,
                None => break,
            };
            self.evict_device_to_host(victim, &mut plan)?;
        }
        let from = self.tier_of(id);
        match from {
            Tier::Host => {
                self.host_used = self.host_used.saturating_sub(bytes);
                self.stats.host_loads += 1;
            }
            Tier::Disk => {
                self.disk_used = self.disk_used.saturating_sub(bytes);
                self.stats.disk_loads += 1;
            }
            Tier::Device => unreachable!("resident segments return early"),
        }
        self.tiers.insert(id, Tier::Device);
        self.device_used = self.device_used.saturating_add(bytes);
        self.touch(id);
        plan.transfers.push(Transfer { segment: id, from, to: Tier::Device, bytes });
        self.stats.transferred_bytes = self.stats.transferred_bytes.saturating_add(bytes);
        Ok(plan)
    }

    /// 预取：只有在不驱逐任何设备段时才会真正换入。
    ///
    /// 返回的计划列出实际换入的段；预算不足的段被跳过，由后续 [`Self::submit`] 处理。
    pub fn prefetch(&mut self, ids: &[SegmentId]) -> Result<LoadPlan, StreamError> {
        let mut plan = LoadPlan::default();
        for id in ids {
            let bytes = self.bytes_of(*id)?;
            if self.tier_of(*id) == Tier::Device {
                continue;
            }
            if bytes > self.device_capacity.saturating_sub(self.device_used) {
                continue;
            }
            let from = self.tier_of(*id);
            match from {
                Tier::Host => {
                    self.host_used = self.host_used.saturating_sub(bytes);
                    self.stats.host_loads += 1;
                }
                Tier::Disk => {
                    self.disk_used = self.disk_used.saturating_sub(bytes);
                    self.stats.disk_loads += 1;
                }
                Tier::Device => continue,
            }
            self.tiers.insert(*id, Tier::Device);
            self.device_used = self.device_used.saturating_add(bytes);
            self.touch(*id);
            self.stats.prefetched_loads += 1;
            plan.transfers.push(Transfer { segment: *id, from, to: Tier::Device, bytes });
            self.stats.transferred_bytes = self.stats.transferred_bytes.saturating_add(bytes);
        }
        Ok(plan)
    }

    /// 缩小设备预算并按 LRU 驱逐到主机，返回被驱逐的段。
    pub fn shrink_device(&mut self, bytes: u64) -> Result<Vec<SegmentId>, StreamError> {
        self.device_capacity = bytes;
        let mut plan = LoadPlan::default();
        while self.device_used > self.device_capacity {
            let victim = match self.lru_in(Tier::Device) {
                Some(victim) => victim,
                None => break,
            };
            self.evict_device_to_host(victim, &mut plan)?;
        }
        Ok(plan.evictions)
    }

    /// 主动把一个设备段释放到主机。
    pub fn release_to_host(&mut self, id: SegmentId) -> Result<(), StreamError> {
        self.bytes_of(id)?;
        if self.tier_of(id) != Tier::Device {
            return Ok(());
        }
        let mut plan = LoadPlan::default();
        self.evict_device_to_host(id, &mut plan)
    }

    /// 段当前所在层级。
    pub fn tier(&self, id: SegmentId) -> Option<Tier> {
        self.tiers.get(&id).copied()
    }

    /// 设备已用字节。
    pub fn device_used(&self) -> u64 {
        self.device_used
    }

    /// 主机已用字节。
    pub fn host_used(&self) -> u64 {
        self.host_used
    }

    /// 磁盘已用字节。
    pub fn disk_used(&self) -> u64 {
        self.disk_used
    }

    /// 累积统计。
    pub fn stats(&self) -> StreamStats {
        self.stats
    }

    fn bytes_of(&self, id: SegmentId) -> Result<u64, StreamError> {
        self.bytes.get(&id).copied().ok_or(StreamError::UnknownSegment(id))
    }

    fn tier_of(&self, id: SegmentId) -> Tier {
        self.tiers.get(&id).copied().unwrap_or(Tier::Disk)
    }

    fn touch(&mut self, id: SegmentId) {
        self.clock += 1;
        self.last_used.insert(id, self.clock);
    }

    fn lru_in(&self, tier: Tier) -> Option<SegmentId> {
        self.tiers
            .iter()
            .filter(|(_, resident)| **resident == tier)
            .map(|(id, _)| *id)
            .min_by_key(|id| self.last_used.get(id).copied().unwrap_or(0))
    }

    fn evict_device_to_host(&mut self, id: SegmentId, plan: &mut LoadPlan) -> Result<(), StreamError> {
        let bytes = self.bytes_of(id)?;
        if self.host_used + bytes > self.host_capacity {
            if let Some(victim) = self.lru_in(Tier::Host) {
                self.evict_host_to_disk(victim, plan)?;
            }
            else {
                return Err(StreamError::HostExhausted { required: bytes, available: self.host_capacity.saturating_sub(self.host_used) });
            }
        }
        self.tiers.insert(id, Tier::Host);
        self.device_used = self.device_used.saturating_sub(bytes);
        self.host_used = self.host_used.saturating_add(bytes);
        self.stats.evictions += 1;
        self.stats.transferred_bytes = self.stats.transferred_bytes.saturating_add(bytes);
        plan.evictions.push(id);
        plan.transfers.push(Transfer { segment: id, from: Tier::Device, to: Tier::Host, bytes });
        Ok(())
    }

    fn evict_host_to_disk(&mut self, id: SegmentId, plan: &mut LoadPlan) -> Result<(), StreamError> {
        let bytes = self.bytes_of(id)?;
        let available = self.disk_capacity.saturating_sub(self.disk_used);
        if bytes > available {
            return Err(StreamError::DiskExhausted { required: bytes, available });
        }
        self.tiers.insert(id, Tier::Disk);
        self.host_used = self.host_used.saturating_sub(bytes);
        self.disk_used = self.disk_used.saturating_add(bytes);
        self.stats.transferred_bytes = self.stats.transferred_bytes.saturating_add(bytes);
        plan.transfers.push(Transfer { segment: id, from: Tier::Host, to: Tier::Disk, bytes });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streamer() -> SegmentStreamer {
        let mut streamer = SegmentStreamer::new(128, 512, 4096);
        for id in 1..=4 {
            streamer.register(SegmentId(id), 64).expect("register");
        }
        streamer
    }

    #[test]
    fn streams_a_model_larger_than_the_device_budget() {
        let mut streamer = streamer();
        streamer.submit(SegmentId(1)).unwrap();
        streamer.submit(SegmentId(2)).unwrap();
        assert_eq!(streamer.device_used(), 128);

        let hit = streamer.submit(SegmentId(1)).unwrap();
        assert!(hit.hit);
        assert!(hit.transfers.is_empty());

        let plan = streamer.submit(SegmentId(3)).unwrap();
        assert_eq!(plan.evictions, vec![SegmentId(2)], "least recently used is evicted");
        assert_eq!(streamer.tier(SegmentId(2)), Some(Tier::Host));
        assert_eq!(streamer.tier(SegmentId(3)), Some(Tier::Device));
        assert_eq!(streamer.device_used(), 128);

        streamer.submit(SegmentId(4)).unwrap();
        assert_eq!(streamer.device_used(), 128);
        let stats = streamer.stats();
        assert_eq!(stats.device_hits, 1);
        assert_eq!(stats.disk_loads, 4);
        assert_eq!(stats.evictions, 2);
    }

    #[test]
    fn rejects_segments_that_can_never_fit_or_are_unknown() {
        let mut streamer = streamer();
        streamer.register(SegmentId(9), 1024).unwrap();
        let error = streamer.submit(SegmentId(9)).expect_err("too large");
        assert!(matches!(error, StreamError::DeviceBudgetTooSmall { .. }));
        assert!(matches!(streamer.submit(SegmentId(77)), Err(StreamError::UnknownSegment(SegmentId(77)))));
        assert!(matches!(streamer.register(SegmentId(1), 1), Err(StreamError::DuplicateSegment(_))));
        assert!(matches!(streamer.register(SegmentId(10), 8192), Err(StreamError::DiskExhausted { .. })));
    }

    #[test]
    fn shrinking_the_budget_evicts_by_lru() {
        let mut streamer = streamer();
        streamer.submit(SegmentId(1)).unwrap();
        streamer.submit(SegmentId(2)).unwrap();
        let evicted = streamer.shrink_device(64).unwrap();
        assert_eq!(evicted, vec![SegmentId(1)]);
        assert_eq!(streamer.device_used(), 64);
        assert_eq!(streamer.stats().evictions, 1);
    }

    #[test]
    fn prefetch_never_evicts_and_marks_later_hits() {
        let mut streamer = SegmentStreamer::new(128, 512, 4096);
        for id in 1..=3 {
            streamer.register(SegmentId(id), 64).unwrap();
        }
        let plan = streamer.prefetch(&[SegmentId(1)]).unwrap();
        assert_eq!(plan.transfers.len(), 1);
        assert_eq!(streamer.tier(SegmentId(1)), Some(Tier::Device));
        assert!(streamer.submit(SegmentId(1)).unwrap().hit);
        assert_eq!(streamer.stats().prefetched_loads, 1);

        streamer.submit(SegmentId(2)).unwrap();
        let plan = streamer.prefetch(&[SegmentId(3)]).unwrap();
        assert!(plan.transfers.is_empty(), "prefetch must not evict resident segments");
    }

    #[test]
    fn host_pressure_spills_to_disk() {
        let mut streamer = SegmentStreamer::new(128, 64, 4096);
        for id in 1..=4 {
            streamer.register(SegmentId(id), 64).unwrap();
        }
        streamer.submit(SegmentId(1)).unwrap();
        streamer.submit(SegmentId(2)).unwrap();
        streamer.submit(SegmentId(3)).unwrap();
        let plan = streamer.submit(SegmentId(4)).unwrap();
        assert_eq!(plan.evictions, vec![SegmentId(2)]);
        assert_eq!(streamer.host_used(), 64);
        assert_eq!(streamer.disk_used(), 64);
        assert!(plan.transfers.iter().any(|transfer| transfer.from == Tier::Host && transfer.to == Tier::Disk));
    }

    #[test]
    fn release_moves_a_device_segment_back_to_host() {
        let mut streamer = streamer();
        streamer.submit(SegmentId(1)).unwrap();
        streamer.release_to_host(SegmentId(1)).unwrap();
        assert_eq!(streamer.tier(SegmentId(1)), Some(Tier::Host));
        assert_eq!(streamer.device_used(), 0);
        assert_eq!(streamer.host_used(), 64);
    }
}
