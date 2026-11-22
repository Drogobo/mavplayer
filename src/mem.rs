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
//!
//! [`frame_buffer`] hands out page-aligned buffers for pictures the GPU
//! writes into, which the presenter pins once (`present::GpuConvert`). They
//! are ordinary `Vec`s to everyone else: freeing one returns it to the pool.

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
                imp::release_idle_frames(std::time::Duration::from_secs(1));
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

/// A buffer of at least `len` bytes (empty, with that capacity) from the
/// pool of page-aligned picture buffers, and whether it was pinned before
/// (see [`mark_pinned`]). `None` when the pool is full or unavailable: use a
/// plain `Vec`. Freeing the `Vec` hands the buffer back to the pool.
pub fn frame_buffer(len: usize) -> Option<(Vec<u8>, bool)> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        imp::frame_buffer(len)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = len;
        None
    }
}

/// Unpins every pool buffer, in use or not (they stay mapped; one in use is
/// pinned again when it next comes round).
pub fn unpin_all() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    imp::unpin_all();
}

/// Records that the pool buffer at `ptr` is now pinned: `unpin` is called
/// for it before it is unmapped.
pub fn mark_pinned(ptr: *const u8, unpin: fn(*mut u8, usize)) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    imp::mark_pinned(ptr, unpin);
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let _ = (ptr, unpin);
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

    /// Undoes the pinning of a buffer (pointer, size).
    type Unpin = fn(*mut u8, usize);

    /// Page-aligned picture buffers mapped here (see `frame_buffer`).
    struct Frame {
        ptr: usize,
        size: usize,
        /// Handed out (a `Vec` owns it) or back in the pool, since when.
        free: Option<Instant>,
        /// Set when the buffer is pinned for GPU downloads (`mark_pinned`).
        unpin: Option<Unpin>,
    }

    const FRAME_SLOTS: usize = 8;
    static FRAMES: Mutex<[Option<Frame>; FRAME_SLOTS]> = Mutex::new([const { None }; FRAME_SLOTS]);

    fn frames() -> std::sync::MutexGuard<'static, [Option<Frame>; FRAME_SLOTS]> {
        FRAMES.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn frame_buffer(len: usize) -> Option<(Vec<u8>, bool)> {
        let size = len.checked_next_multiple_of(4096)?.max(BIG);
        let mut evicted = None;
        let found = {
            let mut g = frames();
            if let Some(f) = g.iter_mut().flatten().find(|f| f.size == size && f.free.is_some()) {
                f.free = None;
                Some((f.ptr, f.unpin.is_some()))
            } else {
                // A free slot, or else the one a buffer of another size
                // (the window was resized) waits in.
                let slot = match g.iter().position(|f| f.is_none()) {
                    Some(i) => Some(i),
                    None => g.iter().position(|f| f.as_ref().is_some_and(|f| f.free.is_some())),
                };
                let slot = slot?;
                evicted = g[slot].take();
                // SAFETY: a fresh anonymous mapping (no allocation happens
                // here, so `dealloc` can't wait on this lock).
                let p = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                if p != libc::MAP_FAILED {
                    g[slot] = Some(Frame { ptr: p as usize, size, free: None, unpin: None });
                    Some((p as usize, false))
                } else {
                    None
                }
            }
        };
        if let Some(f) = evicted {
            unmap(f);
        }
        let (ptr, pinned) = found?;
        // SAFETY: `size` bytes mapped for this `Vec` alone until it is freed,
        // which `dealloc` routes back here.
        Some((unsafe { Vec::from_raw_parts(ptr as *mut u8, 0, size) }, pinned))
    }

    pub fn mark_pinned(ptr: *const u8, unpin: Unpin) {
        if let Some(f) = frames().iter_mut().flatten().find(|f| f.ptr == ptr as usize) {
            f.unpin = Some(unpin);
        }
    }

    pub fn unpin_all() {
        // Unpinned outside the lock: unpinning may free memory.
        let mut pinned: [Option<(usize, usize, Unpin)>; FRAME_SLOTS] = [None; FRAME_SLOTS];
        for (f, out) in frames().iter_mut().flatten().zip(pinned.iter_mut()) {
            if let Some(unpin) = f.unpin.take() {
                *out = Some((f.ptr, f.size, unpin));
            }
        }
        for (ptr, size, unpin) in pinned.into_iter().flatten() {
            unpin(ptr as *mut u8, size);
        }
    }

    /// Takes a pool buffer back. False if `ptr` isn't one.
    fn give_back(ptr: *mut u8) -> bool {
        match frames().iter_mut().flatten().find(|f| f.ptr == ptr as usize) {
            Some(f) => {
                f.free = Some(Instant::now());
                true
            }
            None => false,
        }
    }

    fn unmap(f: Frame) {
        if let Some(unpin) = f.unpin {
            unpin(f.ptr as *mut u8, f.size);
        }
        // SAFETY: mapped by `frame_buffer` and no longer handed out.
        unsafe { libc::munmap(f.ptr as *mut libc::c_void, f.size) };
    }

    /// Unmaps pool buffers back unused for `idle`.
    pub fn release_idle_frames(idle: Duration) {
        let mut stale: [Option<Frame>; FRAME_SLOTS] = [const { None }; FRAME_SLOTS];
        {
            let mut g = frames();
            for (slot, out) in g.iter_mut().zip(stale.iter_mut()) {
                if slot.as_ref().is_some_and(|f| f.free.is_some_and(|t| t.elapsed() >= idle)) {
                    *out = slot.take();
                }
            }
        }
        // Outside the lock: unpinning may allocate and free.
        for f in stale.into_iter().flatten() {
            unmap(f);
        }
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
            if layout.size() >= BIG && give_back(ptr) {
                return;
            }
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
            // A pool buffer moves to an ordinary block.
            if layout.size() >= BIG && frames().iter().flatten().any(|f| f.ptr == ptr as usize) {
                unsafe {
                    let new = System.alloc(Layout::from_size_align_unchecked(new_size, layout.align()));
                    if !new.is_null() {
                        std::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                        give_back(ptr);
                    }
                    return new;
                }
            }
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
