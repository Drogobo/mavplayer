//! Keeping memory use down (Linux, glibc).
//!
//! Playback allocates and frees large buffers all the time: a decoded or
//! converted video frame is 3 to 33 MB. By default glibc serves those from
//! its per-thread arenas and keeps what is freed there, so a dozen threads
//! held 30 to 70 MB more than was in use. Three measures:
//!
//! - `tune` fixes glibc's mmap threshold at 1 MB: big blocks are mapped on
//!   their own and go back to the system when freed.
//! - Mapping and zeroing a fresh frame buffer for every frame costs CPU
//!   (about 5% of a core at 1080p, far more at 4K), so [`Alloc`] keeps a few
//!   freed big blocks for reuse at the same size: a frame buffer is
//!   allocated once and then passed around. Blocks nobody reused within a
//!   second are freed (`start`), so nothing is held once playback stops.
//!   (FFmpeg's frame buffers are pooled the same way; see `decoder.rs`.)
//! - `start` also returns free arena space past a few MB every second, and
//!   closes GPU devices (hardware decoder, upscaler) unused for 30 seconds.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub use imp::Alloc;

/// Sets the allocator up. Call first thing in `main`, before any thread starts.
pub fn tune() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, imp::BIG as libc::c_int);
    }
}

/// Starts the thread that hands unused memory back.
pub fn start() {
    let spawned = std::thread::Builder::new().name("mem-trim".into()).spawn(|| {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            crate::engine::release_idle_devices(std::time::Duration::from_secs(30));
            #[cfg(all(target_os = "linux", target_env = "gnu"))]
            {
                imp::release_idle(std::time::Duration::from_secs(1));
                // SAFETY: plain glibc calls; both lock each arena themselves.
                if unsafe { libc::mallinfo2() }.fordblks > 8 << 20 {
                    unsafe { libc::malloc_trim(0) };
                }
            }
        }
    });
    if let Err(e) = spawned {
        log::debug!("could not start mem-trim: {e}");
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod imp {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Blocks this large are kept for reuse (and mapped on their own by glibc).
    pub const BIG: usize = 1 << 20;
    /// At most this many blocks are kept...
    const SLOTS: usize = 4;
    /// ...and at most this much in all (four 4K frames).
    const MAX_KEPT: usize = 4 * 3840 * 2160 * 4 + (1 << 20);

    struct Kept {
        ptr: *mut u8,
        size: usize,
        since: Instant,
    }

    struct Cache {
        blocks: [Option<Kept>; SLOTS],
        total: usize,
    }

    // SAFETY: the pointers are plain heap blocks owned by the cache.
    unsafe impl Send for Cache {}

    static CACHE: Mutex<Cache> = Mutex::new(Cache { blocks: [const { None }; SLOTS], total: 0 });

    /// The system allocator, plus a few freed big blocks kept for reuse.
    pub struct Alloc;

    fn cacheable(layout: Layout) -> bool {
        layout.size() >= BIG && layout.align() <= 16
    }

    unsafe impl GlobalAlloc for Alloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if cacheable(layout)
                && let Ok(mut c) = CACHE.try_lock()
                && let Some(slot) = c.blocks.iter_mut().find(|b| b.as_ref().is_some_and(|b| b.size == layout.size()))
            {
                let kept = slot.take().unwrap();
                c.total -= kept.size;
                return kept.ptr;
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if cacheable(layout)
                && let Ok(mut c) = CACHE.try_lock()
                && c.total + layout.size() <= MAX_KEPT
            {
                // `Instant::now` doesn't allocate.
                let since = Instant::now();
                let c = &mut *c;
                if let Some(slot) = c.blocks.iter_mut().find(|b| b.is_none()) {
                    *slot = Some(Kept { ptr, size: layout.size(), since });
                    c.total += layout.size();
                    return;
                }
            }
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // Fresh mappings are zeroed already; a kept block would need clearing.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // Kept blocks came from `System` with the same layout, so it can
            // resize any block.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    /// Frees kept blocks nobody reused for `idle`.
    pub fn release_idle(idle: Duration) {
        let mut stale: [Option<Kept>; SLOTS] = [const { None }; SLOTS];
        {
            let Ok(mut c) = CACHE.lock() else { return };
            let c = &mut *c;
            for (slot, out) in c.blocks.iter_mut().zip(stale.iter_mut()) {
                if slot.as_ref().is_some_and(|b| b.since.elapsed() >= idle) {
                    *out = slot.take();
                }
            }
            c.total -= stale.iter().flatten().map(|b| b.size).sum::<usize>();
        }
        for b in stale.into_iter().flatten() {
            // SAFETY: every kept block was allocated by `System` with this
            // size and an alignment of at most 16 (which `System` serves
            // with plain malloc, so align 1 frees it the same way).
            unsafe { System.dealloc(b.ptr, Layout::from_size_align_unchecked(b.size, 1)) };
        }
    }
}
