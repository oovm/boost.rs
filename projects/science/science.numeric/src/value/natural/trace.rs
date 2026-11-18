//! [`Natural`] 的 [`science_gc::Trace`] 实现。

use super::Natural;

impl science_gc::Trace for Natural {
    fn trace(&self, tracer: &mut dyn science_gc::Tracer) {
        if let Some(ptr) = self.inner.heap_ptr() {
            tracer.mark_allocation(ptr.as_ptr().cast());
        }
    }
}
