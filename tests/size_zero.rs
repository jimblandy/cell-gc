//! The GC can allocate objects that are zero-size.

extern crate cell_gc;
#[macro_use]
extern crate cell_gc_derive;
use std::marker::PhantomData;

#[derive(IntoHeap)]
struct Unit<'h> {
    phantom: PhantomData<&'h u8>,
}

fn main() {
    assert_eq!(std::mem::size_of::<UnitStorage>(), 0);

    cell_gc::with_heap(|hs| {
        hs.set_page_limit::<Unit>(Some(1));
        let n = cell_gc::page_capacity::<Unit>();

        // see comment in size_medium.rs
        assert!(n >= 500);
        assert!(n <= 1024);

        let _: Vec<UnitRef> = (0..n)
            .map(|_| hs.alloc(Unit { phantom: PhantomData }))
            .collect();

        hs.force_gc();

        assert_eq!(hs.try_alloc(Unit { phantom: PhantomData }), None);
    });
}
