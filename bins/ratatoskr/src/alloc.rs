//! Process allocator. Background threads purge unused pages, so a finished
//! pass can return memory while the thread that allocated it is idle.

use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

pub(crate) struct Heap {
    pub allocated: usize,
    pub active: usize,
    pub resident: usize,
    pub retained: usize,
}

/// Bytes the application owns, bytes in active pages, bytes resident, and
/// bytes still mapped after a purge. Statistics refresh when the epoch advances.
pub(crate) fn heap() -> Option<Heap> {
    tikv_jemalloc_ctl::epoch::advance().ok()?;
    Some(Heap {
        allocated: tikv_jemalloc_ctl::stats::allocated::read().ok()?,
        active: tikv_jemalloc_ctl::stats::active::read().ok()?,
        resident: tikv_jemalloc_ctl::stats::resident::read().ok()?,
        retained: tikv_jemalloc_ctl::stats::retained::read().ok()?,
    })
}
