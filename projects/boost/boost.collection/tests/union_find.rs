//! `boost-collection` 并查集验收：合并语义、代表元稳定、路径压缩后的
//! 查询一致、深拷贝独立演化，以及深链不依赖递归栈。

use boost_collection::UnionFind;

#[test]
fn singletons_merge_and_report_set_count() {
    let mut sets = UnionFind::new(4);
    assert_eq!(sets.len(), 4);
    assert_eq!(sets.set_count(), 4);
    assert!(sets.union(0, 1));
    assert!(sets.union(2, 3));
    assert_eq!(sets.set_count(), 2);
    assert!(!sets.union(0, 1), "重复合并必须报告未变化");
    assert_eq!(sets.find(0), sets.find(1));
    assert_ne!(sets.find(0), sets.find(2));
    assert!(sets.union(1, 2));
    assert_eq!(sets.set_count(), 1);
    assert_eq!(sets.find(3), sets.find(0));
}

#[test]
fn zero_and_one_element_partitions_are_well_defined() {
    let mut empty = UnionFind::new(0);
    assert!(empty.is_empty());
    assert_eq!(empty.set_count(), 0);

    let mut single = UnionFind::new(1);
    assert_eq!(single.find(0), 0);
    assert_eq!(single.set_count(), 1);
    assert!(!single.union(0, 0), "自合并不是变化");
}

#[test]
fn clone_diverges_from_the_original() {
    let mut sets = UnionFind::new(3);
    sets.union(0, 1);
    let mut copy = sets.clone();
    assert_eq!(copy.find(0), copy.find(1));
    copy.union(1, 2);
    assert_eq!(copy.set_count(), 1);
    assert_eq!(sets.set_count(), 2, "拷贝后的合并不得回流原件");
}

#[test]
fn deep_chain_resolves_without_recursion() {
    // 构造 0-1-2-…-99_999 的长链：迭代 find 不受调用栈深度影响。
    let mut sets = UnionFind::new(100_000);
    for index in 1..100_000 {
        sets.union(index - 1, index);
    }
    assert_eq!(sets.find(99_999), sets.find(0));
    assert_eq!(sets.set_count(), 1);
    // 压缩后再查询保持同一代表元。
    assert_eq!(sets.find(50_000), sets.find(99_999));
}
