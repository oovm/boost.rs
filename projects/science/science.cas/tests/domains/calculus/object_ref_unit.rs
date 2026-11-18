//! 自 `src/domains/calculus/object_ref.rs` 迁出的原内联测试。

use science_cas::domains::calculus::{Remainder, Series, SeriesObjectStore};

#[test]
fn intern_dedupes_identical_series() {
    let mut store = SeriesObjectStore::new();
    let s = Series {
        variable: science_types::SymbolId(0),
        center: science_types::TermId(0),
        terms: Vec::new(),
        order: 1,
        remainder: Remainder::ExactTruncation,
    };
    let a = store.intern(s.owning_copy());
    let b = store.intern(s);
    assert_eq!(a, b);
    assert_eq!(store.len(), 1);
    assert!(store.object_ref(a).is_some());
}
