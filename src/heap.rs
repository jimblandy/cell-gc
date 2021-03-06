//! GC heap allocator.
//!
//! ### Safety
//!
//! First, a few general observations about safety in Rust.
//!
//! *   If a `mut` reference to a value exists, and other references to the value
//!     also exist, we can definitely crash.
//!
//!     One way it can happen is that the `mut` value is or contains an
//!     `enum`. Using the other reference, we can borrow a refrence to a
//!     current field of the `enum`. Then, using the `mut` reference, we can
//!     assign some other variant to the `enum`, invalidating the reference.
//!     Rust does not detect that the reference is invalid, so when we use it,
//!     we are accessing gibberish.
//!
//!     Another way is if a callback is called while we're in the middle of
//!     mutating a value, while it's in an invalid state. When you read
//!     "callback", think of `deref` and `drop` methods, which are called
//!     implicitly all over the place. These callbacks are normally safe,
//!     because they simply can't have a reference to the value that's in an
//!     intermediate state. But if another reference exists, it might be used
//!     to try to read from the half-mutated value.
//!
//! *   If a data structure contains two paths to a value, under Rust's usual
//!     rules, you can get two `mut` references to that value.
//!
//!     This is why you tend to build ownership trees in Rust and it's why GC
//!     is particularly challenging in Rust: GCs build an arbitrary graph of
//!     values and references.
//!
//! This GC takes the following approach to ensure safety.
//!
//! *   Minimize access to values stored in the heap. For the most part,
//!     application code *never* sees a direct reference to any value that is
//!     physically stored someplace where it's subject to GC.
//!
//! *   Minimize the times when direct references to in-heap values exist at
//!     all, and during these operations, prevent control from escaping to
//!     arbitrary application code.
//!
//! *   Ensure that when any direct references to in-heap values exist, they
//!     obey Rust's rules: for any given value, either only non-`mut`
//!     references, or at most one `mut` reference, exists at a time.
//!
//! Thus we are particularly keen to avoid the possibility of "reentering" the
//! heap, creating new references to in-heap values while others already exist.
//!
//! References to heap values therefore exist only during the following
//! operations:
//!
//! *   Allocation - That is, moving values into the heap. This is safe because
//!     it never triggers any user code at all while heap references exist.
//!
//! * - Heap reads and writes - The only way to do these is via macro-generated
//!     accessors which do not expose references.  Reads call `from_heap()` on
//!     in-heap values, which is dangerous because `from_heap()` receives a
//!     direct reference.  Writes call `drop()`, which is even more dangerous:
//!     (1) it receives a direct `mut` reference; and (2) it leaves in-heap
//!     values uninitialized.
//!
//! *   GC marking - The code for this is all macro-generated.
//!
//! *   GC sweeping - This calls `drop()`, which is dangerous for the reasons
//!     noted above.
//!
//! To make this scheme safe, `from_heap()` and `drop()` must be tightly controlled.
//! `from_heap()` is therefore in an unsafe trait; users are expected to use
//! the `gc_heap_type!` to autogenerate instances.
//!
//! However, **we leave it up to the user to exercise care with `drop()`.**
//! We suggest *never* implementing `Drop` for a heap type. If you must,
//! avoid reading pointer fields while dropping, and avoid calling into
//! arbitrary code.

use gcref::GcRef;
use marking::{MarkingTracer, mark};
use pages::{PageSet, PageSetRef, TypedPage, heap_type_id};
use ptr::{Pointer, UntypedPointer};
use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::ptr;
use traits::IntoHeapAllocation;
use pages::TypeId;

/// A `Heap` is a universe in which you can store values that implement
/// `IntoHeapAllocation`. The values are mutable and they can point to each
/// other, in cycles.
pub struct Heap {
    /// Map from heap types to the set of pages for that type.
    page_sets: HashMap<TypeId, PageSet>,

    /// The root set. This tracks allocations that are "pinned", referred to
    /// from outside the heap.
    pins: RefCell<HashMap<UntypedPointer, usize>>,

    /// Tracer for the mark phase of GC.
    marking_tracer: Option<MarkingTracer>,
}

// What does this do? You'll never guess!
pub type HeapSessionId<'h> = PhantomData<::std::cell::Cell<&'h mut ()>>;

pub struct HeapSession<'h> {
    id: HeapSessionId<'h>,
    heap: &'h mut Heap,
}

/// Create a heap, pass it to a callback, then destroy the heap.
///
/// The heap's lifetime is directly tied to this function call, for safety. (So
/// the API is a little wonky --- but how many heaps were you planning on
/// creating?)
pub fn with_heap<R, F>(f: F) -> R
where
    F: for<'h> FnOnce(&mut HeapSession<'h>) -> R,
{
    Heap::new().enter(f)
}

impl Heap {
    /// Create a new, empty heap.
    pub fn new() -> Heap {
        Heap {
            page_sets: HashMap::new(),
            pins: RefCell::new(HashMap::new()),
            marking_tracer: Some(MarkingTracer::default()),
        }
    }

    /// Start a session to access this heap.
    ///
    /// You need a `HeapSession` in order to do anything interesting with a heap.
    /// Each heap has either 0 or 1 `HeapSession` at a time, and a `HeapSession`
    /// is bound to a stack lifetime, so Rust can enforce safety rules.
    ///
    /// It would be safe to make this method public, but it's a pain in
    /// practice. You'll want to pass a `&mut HeapSession<'h>` around, not a
    /// `HeapSession<'h>`, since `HeapSession` is not `Copy`. Use `enter`.
    fn open<'h>(&'h mut self) -> HeapSession<'h> {
        HeapSession {
            id: PhantomData,
            heap: self,
        }
    }

    /// Run some code using this Heap.
    ///
    /// # Example
    ///
    ///     use cell_gc::{Heap, GCLeaf};
    ///
    ///     let mut heap = Heap::new();
    ///     heap.enter(|hs| {
    ///         // ... hs.alloc(MyHeapStruct { ... }) ...
    ///         # hs.force_gc();
    ///     });
    ///
    pub fn enter<R, F>(&mut self, f: F) -> R
    where
        F: for<'h> FnOnce(&mut HeapSession<'h>) -> R,
    {
        f(&mut self.open())
    }

    /// Add the value `*p` to the root set, protecting it from GC.
    ///
    /// A value that has been pinned *n* times stays in the root set
    /// until it has been unpinned *n* times.
    ///
    /// # Safety
    ///
    /// `p` must point to a live allocation of type `T` in this heap.
    pub unsafe fn pin<'h, T: IntoHeapAllocation<'h>>(&self, p: Pointer<T::In>) {
        let mut pins = self.pins.borrow_mut();
        let entry = pins.entry(p.into()).or_insert(0);
        *entry += 1;
    }

    /// Unpin a heap-allocated value (see `pin`).
    ///
    /// # Safety
    ///
    /// `p` must point to a pinned allocation of type `T` in this heap.
    pub unsafe fn unpin<'h, T: IntoHeapAllocation<'h>>(&self, p: Pointer<T::In>) {
        let mut pins = self.pins.borrow_mut();
        let done = {
            let entry = pins.entry(p.into()).or_insert(0);
            assert!(*entry != 0);
            *entry -= 1;
            *entry == 0
        };
        if done {
            pins.remove(&p.into());
        }
    }

    /// Call the given function on each pinned root.
    pub fn each_pin<F>(&self, mut f: F)
    where
        F: FnMut(UntypedPointer),
    {
        for (&ptr, _) in self.pins.borrow().iter() {
            f(ptr);
        }
    }

    pub unsafe fn from_allocation<'h, T: IntoHeapAllocation<'h>>(
        ptr: Pointer<T::In>,
    ) -> *const Heap {
        (*TypedPage::find(ptr)).header.heap
    }

    pub unsafe fn get_mark_bit<'h, T: IntoHeapAllocation<'h>>(ptr: Pointer<T::In>) -> bool {
        (*TypedPage::find(ptr)).get_mark_bit(ptr)
    }

    pub unsafe fn set_mark_bit<'h, T: IntoHeapAllocation<'h>>(ptr: Pointer<T::In>) {
        (*TypedPage::find(ptr)).set_mark_bit(ptr);
    }

    fn take_marking_tracer(&mut self) -> MarkingTracer {
        self.marking_tracer.take().unwrap()
    }

    fn replace_marking_tracer(&mut self, tracer: MarkingTracer) {
        assert!(self.marking_tracer.is_none());
        assert!(tracer.mark_stack_is_empty());
        self.marking_tracer = Some(tracer);
    }

    /// Run the given function with the marking tracer.
    ///
    /// The marking tracer is taken out of the heap and replaced again so we can
    /// have two independent borrows of the heap and the marking tracer and the
    /// same time.
    pub(crate) fn with_marking_tracer<F, O>(&mut self, mut f: F) -> O
    where
        F: FnMut(&mut Self, &mut MarkingTracer) -> O,
    {
        let mut tracer = self.take_marking_tracer();
        let retval = f(self, &mut tracer);
        self.replace_marking_tracer(tracer);
        retval
    }

    /// Clear all mark bits in preparation for GC.
    ///
    /// # Safety
    ///
    /// This must be called only at the beginning of a GC cycle.
    pub(crate) unsafe fn clear_mark_bits(&mut self) {
        for page_set in self.page_sets.values_mut() {
            page_set.clear_mark_bits();
        }
    }

    fn gc(&mut self) {
        mark(self);

        // sweep phase
        for page_set in self.page_sets.values_mut() {
            unsafe {
                page_set.sweep();
            }
        }
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // Perform a final GC to call destructors on any remaining allocations.
        assert!(self.pins.borrow().is_empty());
        self.gc();

        for page_set in self.page_sets.values() {
            page_set.assert_no_allocations();
        }
    }
}

impl<'h> HeapSession<'h> {
    fn get_page_set<'a, T: IntoHeapAllocation<'h> + 'a>(&'a mut self) -> PageSetRef<'a, 'h, T> {
        let key = heap_type_id::<T>();
        let heap: *mut Heap = self.heap;
        self.heap
            .page_sets
            .entry(key)
            .or_insert_with(|| unsafe { PageSet::new::<T>(heap) })
            .downcast_mut()
    }

    /// Set (or unset) the limit on the number of pages that can be used to
    /// allocate values of type `T` in this heap. By default, no limit is set.
    ///
    /// See `try_alloc` for more.
    ///
    /// If there are already at least `limit` pages for `T` values, this may have no effect;
    /// it doesn't cause pages to be freed.
    pub fn set_page_limit<T: IntoHeapAllocation<'h>>(&mut self, limit: Option<usize>) {
        self.get_page_set::<T>().set_page_limit(limit);
    }

    /// Allocate memory, moving `value` into the heap.
    ///
    /// If a limit has previously been set using `set_page_limit`, and we run
    /// up against the limit (already have at least that many pages for `T`
    /// values, and they are all full of live values), `try_alloc` first
    /// attempts to free some memory by doing garbage collection. If that
    /// doesn't work, `try_alloc` returns `None`.
    pub fn try_alloc<T: IntoHeapAllocation<'h>>(&mut self, value: T) -> Option<T::Ref> {
        // For now, this is done very early, so that if it panics, the heap is
        // left in an OK state. Better wrapping of raw pointers would make it
        // possible to do this later, closer to the `ptr::write()` call. (And
        // the compiler might optimize away this temporary if we do it that
        // way.) Looking forward to placement new!
        let u = value.into_heap();
        unsafe {
            let alloc = self.get_page_set::<T>().try_alloc();
            alloc
                .or_else(|| {
                    self.heap.gc();
                    self.get_page_set::<T>().try_alloc()
                })
                .map(move |p| {
                    ptr::write(p.as_raw() as *mut _, u);
                    T::wrap_gcref(GcRef::new(p))
                })
        }
    }

    /// Allocate memory, moving `T` into the heap. This may cause garbage collection.
    ///
    /// # Panics
    ///
    /// If a page limit has been set, all pages are full, and GC fails to shake
    /// anything loose.
    pub fn alloc<T: IntoHeapAllocation<'h>>(&mut self, value: T) -> T::Ref {
        self.try_alloc(value)
            .expect("out of memory (gc did not collect anything)")
    }

    /// Do garbage collection.
    pub fn force_gc(&mut self) {
        self.heap.gc();
    }
}
