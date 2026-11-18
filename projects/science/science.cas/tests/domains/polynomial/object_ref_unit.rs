//! 自 `src/domains/polynomial/object_ref.rs` 迁出的原内联测试。

use science_cas::{
    Session,
    domains::polynomial::*,
    reasoning::mgraph::{ObjectRef, TheoryContextId},
};
use science_types::{Diagnostic, DiagnosticCode, Result, RingId};

#[test]
fn intern_dedupes_by_fingerprint() {
    let rings = RingTable::default();
    let mut store = PolynomialObjectStore::new();
    let a = store.intern(Polynomial::zero(RingId(0)), &rings);
    let b = store.intern(Polynomial::zero(RingId(0)), &rings);
    assert_eq!(a, b);
    assert_eq!(store.len(), 1);
    assert!(store.object_ref(a).is_some());
}
