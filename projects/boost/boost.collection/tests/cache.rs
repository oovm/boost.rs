//! `boost-collection` 内存 LRU 映射的基础验收。
//!
//! 覆盖：容量零/一、命中提升与 peek 不提升、替换与淘汰的所有权交还、
//! 删除与缩容的顺序不变量、MRU → LRU 迭代顺序、非 Copy 值恰好释放一次，
//! 以及完整键 Eq 身份（哈希碰撞不视为同一键）。

use boost_collection::{LruCache, PutOutcome};
use std::{
    cell::Cell,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    rc::Rc,
};

#[test]
fn zero_capacity_rejects_every_entry() {
    let mut cache: LruCache<u32, u32> = LruCache::new(0);
    assert_eq!(cache.put(1, 10), PutOutcome::Rejected((1, 10)));
    assert!(cache.is_empty());
    assert_eq!(cache.get(&1), None);
    assert_eq!(cache.capacity(), 0);
}

#[test]
fn hits_promote_while_peek_and_misses_do_not() {
    let mut cache = LruCache::new(3);
    for key in [1, 2, 3] {
        assert_eq!(cache.put(key, key * 10), PutOutcome::Inserted);
    }
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![3, 2, 1]);

    // peek 不提升：LRU 仍是 1。
    assert_eq!(cache.peek(&1), Some(&10));
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![3, 2, 1]);

    // 命中提升：1 变为 MRU。
    assert_eq!(cache.get(&1), Some(&10));
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![1, 3, 2]);

    // 未命中不改变顺序。
    assert_eq!(cache.get(&9), None);
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![1, 3, 2]);

    assert_eq!(cache.put(4, 40), PutOutcome::Evicted((2, 20)));
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![4, 1, 3]);
}

#[test]
fn replacement_promotes_without_changing_capacity() {
    let mut cache = LruCache::new(2);
    assert_eq!(cache.put("a", 1), PutOutcome::Inserted);
    assert_eq!(cache.put("b", 2), PutOutcome::Inserted);
    assert_eq!(cache.put("a", 3), PutOutcome::Replaced(1));
    assert_eq!(cache.len(), 2);
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec!["a", "b"]);
    assert_eq!(cache.put("c", 4), PutOutcome::Evicted(("b", 2)));
    assert_eq!(cache.get(&"a"), Some(&3));
}

#[test]
fn remove_and_resize_keep_the_order_invariant() {
    let mut cache = LruCache::new(4);
    for key in [1, 2, 3, 4] {
        cache.put(key, key);
    }
    assert_eq!(cache.remove(&2), Some(2));
    assert_eq!(cache.remove(&2), None);
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![4, 3, 1]);

    // 缩容按 LRU 顺序淘汰并交还所有权。
    assert_eq!(cache.resize(1), vec![(1, 1), (3, 3)]);
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![4]);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.capacity(), 1);

    // 扩容不改变现有顺序，之后继续按新容量淘汰。
    assert!(cache.resize(3).is_empty());
    cache.put(5, 5);
    cache.put(6, 6);
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![6, 5, 4]);
    cache.put(7, 7);
    assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![7, 6, 5]);

    assert_eq!(cache.resize(0), vec![(5, 5), (6, 6), (7, 7)]);
    assert!(cache.is_empty());
    assert_eq!(cache.get(&7), None);
}

#[test]
fn clear_drops_values_and_keeps_capacity() {
    let dropped = Rc::new(Cell::new(0));

    struct Token(Rc<Cell<usize>>);
    impl Drop for Token {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let mut cache = LruCache::new(2);
    cache.put(1, Token(Rc::clone(&dropped)));
    cache.put(2, Token(Rc::clone(&dropped)));
    assert!(matches!(cache.put(3, Token(Rc::clone(&dropped))), PutOutcome::Evicted(_)));
    assert_eq!(dropped.get(), 1, "the evicted value is dropped exactly once");

    // 被替换的旧值同样只释放一次。
    if let PutOutcome::Replaced(previous) = cache.put(2, Token(Rc::clone(&dropped))) {
        drop(previous);
    }
    assert_eq!(dropped.get(), 2);

    cache.clear();
    assert_eq!(dropped.get(), 4);
    assert_eq!(cache.capacity(), 2);
    assert!(cache.is_empty());
}

/// 故意让所有键碰撞到同一个哈希值，验证命中仍比较完整 `Eq`。
#[derive(Clone, Debug)]
struct Colliding(u32);

impl PartialEq for Colliding {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Colliding {}

impl Hash for Colliding {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        DefaultHasher::new().finish().hash(state);
    }
}

#[test]
fn full_key_equality_decides_identity_even_on_hash_collisions() {
    let mut cache = LruCache::new(4);
    cache.put(Colliding(1), "one");
    cache.put(Colliding(2), "two");
    assert_eq!(cache.get(&Colliding(1)), Some(&"one"));
    assert_eq!(cache.get(&Colliding(2)), Some(&"two"));
    assert_eq!(cache.get(&Colliding(3)), None);
    assert_eq!(cache.len(), 2);
}

#[test]
fn eviction_follows_a_reference_model_under_random_operations() {
    // 简单参考模型：Vec 头部为 MRU，尾部为 LRU。
    let mut cache = LruCache::new(8);
    let mut model: Vec<(u32, u32)> = Vec::new();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;

    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for _ in 0..2000 {
        let key = (next() % 16) as u32;
        match next() % 4 {
            0 => {
                let value = (next() % 100) as u32;
                let outcome = cache.put(key, value);
                if let Some(position) = model.iter().position(|(existing, _)| *existing == key) {
                    assert_eq!(outcome, PutOutcome::Replaced(model.remove(position).1));
                }
                else {
                    assert!(matches!(outcome, PutOutcome::Inserted | PutOutcome::Evicted(_)));
                    if model.len() == 8 {
                        model.pop();
                    }
                }
                model.insert(0, (key, value));
            }
            1 => {
                let expected = model.iter().find(|(existing, _)| *existing == key).map(|(_, value)| *value);
                let actual = cache.get(&key).copied();
                assert_eq!(actual, expected);
                if let Some(position) = model.iter().position(|(existing, _)| *existing == key) {
                    let entry = model.remove(position);
                    model.insert(0, entry);
                }
            }
            2 => {
                let expected = model.iter().find(|(existing, _)| *existing == key).map(|(_, value)| *value);
                assert_eq!(cache.remove(&key), expected);
                if let Some(position) = model.iter().position(|(existing, _)| *existing == key) {
                    model.remove(position);
                }
            }
            _ => {
                let expected = model.iter().find(|(existing, _)| *existing == key).map(|(_, value)| *value);
                assert_eq!(cache.peek(&key).copied(), expected);
            }
        }

        assert_eq!(cache.len(), model.len());
        assert_eq!(cache.iter().map(|(key, value)| (*key, *value)).collect::<Vec<_>>(), model);
    }
}
