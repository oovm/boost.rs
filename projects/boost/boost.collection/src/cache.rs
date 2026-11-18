//! 有容量约束的内存 LRU 映射。
//!
//! 本模块只拥有容量、访问顺序与淘汰机制；翻译内容修订、HTTP 缓存有效性、
//! 编译产物失效和数据库页脏写仍由各自领域拥有。不提供 TTL、权重淘汰、
//! 持久化、分布式后端或全局缓存服务。

use core::{
    fmt::{self, Debug, Formatter},
    hash::Hash,
};
use std::collections::HashMap;

/// [`LruCache::put`] 的结果，保留被替换、淘汰或拒绝条目的所有权。
///
/// 插入新键且未达容量时为 [`Inserted`](Self::Inserted)；已有键换值时为
/// [`Replaced`](Self::Replaced)，不改变条目数；插入新键且已满时淘汰最久未
/// 使用条目，两者同时交还；容量为零时条目不被保留，原样交还。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PutOutcome<K, V> {
    /// 新键写入且未触发淘汰。
    Inserted,
    /// 已有键被替换，返回旧值。
    Replaced(V),
    /// 新键写入并淘汰最久未使用条目，返回被淘汰的键与值。
    Evicted((K, V)),
    /// 容量为零，条目未被保留，原样返回。
    Rejected((K, V)),
}

struct Entry<K, V> {
    key: K,
    value: V,
    /// 指向更近访问的槽位，`None` 表示本条目就是 MRU。
    newer: Option<usize>,
    /// 指向更久未访问的槽位，`None` 表示本条目就是 LRU。
    older: Option<usize>,
}

/// 容量按条目数计算的内存 LRU 映射。
///
/// - 容量为零表示禁用缓存：任何 `put` 都原样返回条目，`get` 恒为 `None`，
///   不会 panic，也不会悄悄改成容量 1 或无限。
/// - [`get`](Self::get)、[`get_mut`](Self::get_mut) 与替换已有键的 `put`
///   都会把命中的条目提升为最近使用；[`peek`](Self::peek) 只读不提升；
///   未命中不改变顺序。
/// - 迭代顺序固定为 MRU → LRU，与 `HashMap` 的迭代顺序无关。
/// - 查找、提升与单项淘汰期望 O(1)：哈希索引定位槽位，槽位内保存前后链，
///   不以每次命中扫描 `Vec` 或双向队列的方式实现。
///
/// 键在哈希索引与槽位中各保存一份：索引副本用于按借用键查找，槽位副本
/// 用于迭代与淘汰时交还所有权，因此内存成本约为两份额外键。哈希只用于
/// 定位，命中后仍比较完整 `Eq`，即完整键相等才算同一缓存条目。
///
/// 所有操作都是安全 Rust：内部键索引、前后链、空闲槽位与长度同步维护。
/// 调用方的 `Hash`/`Eq`/`Clone` 若 panic，操作可能提前中断并使该次写入的
/// 槽位不可达，但不会产生悬垂索引或未定义行为。
pub struct LruCache<K, V> {
    capacity: usize,
    index: HashMap<K, usize>,
    slots: Vec<Option<Entry<K, V>>>,
    free: Vec<usize>,
    /// 最近使用的槽位。
    head: Option<usize>,
    /// 最久未使用的槽位。
    tail: Option<usize>,
    len: usize,
}

impl<K, V> LruCache<K, V> {
    /// 建立容量（条目数）为 `capacity` 的空缓存；零容量表示禁用缓存。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self { capacity, index: HashMap::new(), slots: Vec::new(), free: Vec::new(), head: None, tail: None, len: 0 }
    }

    /// 当前容量（条目数）。
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前条目数。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// 是否没有条目。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 按 MRU → LRU 顺序迭代条目，不改变访问顺序。
    pub fn iter(&self) -> LruIter<'_, K, V> {
        LruIter { cache: self, cursor: self.head }
    }

    /// 按 MRU → LRU 顺序迭代键，不改变访问顺序。
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(key, _)| key)
    }

    /// 读取并提升为最近使用；未命中返回 `None` 且不改变顺序。
    pub fn peek(&self, key: &K) -> Option<&V>
    where
        K: Eq + Hash,
    {
        self.index.get(key).and_then(|slot| self.slots[*slot].as_ref()).map(|entry| &entry.value)
    }
}

impl<K: Eq + Hash + Clone, V> LruCache<K, V> {
    /// 读取并提升为最近使用；未命中返回 `None` 且不改变顺序。
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let slot = *self.index.get(key)?;
        self.promote(slot);
        self.slots[slot].as_ref().map(|entry| &entry.value)
    }

    /// 读取可变引用并提升为最近使用；未命中返回 `None` 且不改变顺序。
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let slot = *self.index.get(key)?;
        self.promote(slot);
        self.slots[slot].as_mut().map(|entry| &mut entry.value)
    }

    /// 写入条目并返回被替换、淘汰或拒绝的内容。
    ///
    /// 替换已有键不改变条目数并提升该条目；新键在满容量时淘汰 LRU 条目；
    /// 零容量时原样返回传入的键与值。
    pub fn put(&mut self, key: K, value: V) -> PutOutcome<K, V> {
        if self.capacity == 0 {
            return PutOutcome::Rejected((key, value));
        }

        if let Some(&slot) = self.index.get(&key) {
            let entry = self.slots[slot].as_mut().expect("an indexed slot is occupied");
            let replaced = core::mem::replace(&mut entry.value, value);
            self.promote(slot);
            return PutOutcome::Replaced(replaced);
        }

        // 先复制索引键再淘汰：复制失败时缓存尚未发生任何变更。
        let indexed = key.clone();
        let evicted = if self.len == self.capacity { self.pop_lru() } else { None };

        let slot = self.allocate(Entry { key, value, newer: None, older: None });
        self.index.insert(indexed, slot);
        self.attach_front(slot);
        self.len += 1;

        match evicted {
            Some(entry) => PutOutcome::Evicted(entry),
            None => PutOutcome::Inserted,
        }
    }

    /// 删除条目并返回其值；不存在时返回 `None`。
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.index.remove(key)?;
        self.detach(slot);
        self.len -= 1;
        let entry = self.slots[slot].take().expect("an indexed slot is occupied");
        self.free.push(slot);
        Some(entry.value)
    }

    /// 淘汰并返回最久未使用的条目。
    pub fn pop_lru(&mut self) -> Option<(K, V)> {
        let slot = self.tail?;
        let key = &self.slots[slot].as_ref().expect("a linked slot is occupied").key;
        self.index.remove(key);

        self.detach(slot);
        self.len -= 1;
        let entry = self.slots[slot].take().expect("a linked slot is occupied");
        self.free.push(slot);
        Some((entry.key, entry.value))
    }

    /// 调整容量并按需淘汰最久未使用条目，返回被淘汰的条目（LRU 在前）。
    ///
    /// 扩容不改变现有顺序；缩容到零等价于清空。
    pub fn resize(&mut self, capacity: usize) -> Vec<(K, V)> {
        self.capacity = capacity;
        let mut evicted = Vec::new();
        while self.len > self.capacity {
            match self.pop_lru() {
                Some(entry) => evicted.push(entry),
                None => break,
            }
        }
        evicted
    }

    /// 清空所有条目并保留容量设置。
    pub fn clear(&mut self) {
        self.index.clear();
        self.slots.clear();
        self.free.clear();
        self.head = None;
        self.tail = None;
        self.len = 0;
    }
}

impl<K, V> LruCache<K, V> {
    /// 取得空闲槽位；已释放的槽位优先复用，避免无界增长。
    fn allocate(&mut self, entry: Entry<K, V>) -> usize {
        match self.free.pop() {
            Some(slot) => {
                self.slots[slot] = Some(entry);
                slot
            }
            None => {
                self.slots.push(Some(entry));
                self.slots.len() - 1
            }
        }
    }

    /// 把槽位挂到 MRU 端，要求它当前不在链上。
    fn attach_front(&mut self, slot: usize) {
        let older = self.head;
        {
            let entry = self.slots[slot].as_mut().expect("an allocated slot is occupied");
            entry.newer = None;
            entry.older = older;
        }
        match older {
            Some(head) => self.slots[head].as_mut().expect("a linked slot is occupied").newer = Some(slot),
            None => self.tail = Some(slot),
        }
        self.head = Some(slot);
    }

    /// 把槽位从链上摘下，两侧邻居直接相连。
    fn detach(&mut self, slot: usize) {
        let (newer, older) = {
            let entry = self.slots[slot].as_ref().expect("a linked slot is occupied");
            (entry.newer, entry.older)
        };
        match newer {
            Some(newer) => self.slots[newer].as_mut().expect("a linked slot is occupied").older = older,
            None => self.head = older,
        }
        match older {
            Some(older) => self.slots[older].as_mut().expect("a linked slot is occupied").newer = newer,
            None => self.tail = newer,
        }
        let entry = self.slots[slot].as_mut().expect("a linked slot is occupied");
        entry.newer = None;
        entry.older = None;
    }

    /// 把已在链上的槽位提升为 MRU。
    fn promote(&mut self, slot: usize) {
        if self.head == Some(slot) {
            return;
        }
        self.detach(slot);
        self.attach_front(slot);
    }
}

impl<K, V> Debug for LruCache<K, V> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("LruCache").field("capacity", &self.capacity).field("len", &self.len).finish_non_exhaustive()
    }
}

/// [`LruCache::iter`] 的迭代器，按 MRU → LRU 顺序交出条目。
pub struct LruIter<'a, K, V> {
    cache: &'a LruCache<K, V>,
    cursor: Option<usize>,
}

impl<'a, K, V> Iterator for LruIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let slot = self.cursor?;
        let entry = self.cache.slots[slot].as_ref()?;
        self.cursor = entry.older;
        Some((&entry.key, &entry.value))
    }
}

impl<'a, K, V> IntoIterator for &'a LruCache<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = LruIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
