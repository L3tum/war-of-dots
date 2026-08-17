//! Counting global allocator for benchmark allocation profiling.
//! Active only under `--features bench-alloc`.
//! Wraps `std::alloc::System` (the default allocator) with Relaxed atomics.
//! Overhead: ~1 atomic add per allocation (~0.5 ns).
//!
//! NOT compatible with Miri (replaces the global allocator).
//! Mutually exclusive with dhat (both install a global allocator).

#![cfg(feature = "bench-alloc")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

struct CountingAlloc;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
/// Largest single allocation (bytes) seen since the last `snapshot()` call.
/// Reset to 0 by each snapshot; `alloc`/`realloc` bump it via atomic-max.
static MAX_SINCE_SNAPSHOT: AtomicUsize = AtomicUsize::new(0);

// Safety: CountingAlloc delegates all actual allocation to std::alloc::System,
// which is a sound GlobalAlloc. The atomic counters are Relaxed-ordered and
// only affect benchmark reporting, not memory safety.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if p.is_null() {
            return p;
        }
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        MAX_SINCE_SNAPSHOT.fetch_max(layout.size(), Ordering::Relaxed);
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // We don't decrement — we track total alloc calls and total bytes churned.
        // This is what the user wants: "how many allocations does this tick make?"
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if p.is_null() {
            return p;
        }
        // Count the size delta as new allocation activity
        let delta = new_size.saturating_sub(layout.size());
        if delta > 0 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(delta, Ordering::Relaxed);
        }
        // Track the resulting size as a candidate for the biggest single alloc.
        MAX_SINCE_SNAPSHOT.fetch_max(new_size, Ordering::Relaxed);
        p
    }
}

/// Point-in-time read of the cumulative counters.
#[derive(Clone, Copy, Default)]
pub struct AllocSnapshot {
    /// Total alloc calls since process start.
    pub allocs: u64,
    /// Total bytes allocated since process start.
    pub bytes: u64,
    /// Largest single allocation (bytes) that occurred since the previous
    /// `snapshot()` call. Captured (and the counter reset) at snapshot time.
    pub max_alloc: u64,
}

/// Take a snapshot of the current cumulative allocation counters.
///
/// Also captures-and-resets the "biggest single allocation since the last
/// snapshot" window, so `max_alloc` is the peak single alloc in the region
/// preceding this call.
pub fn snapshot() -> AllocSnapshot {
    AllocSnapshot {
        allocs: ALLOCS.load(Ordering::Relaxed) as u64,
        bytes: BYTES.load(Ordering::Relaxed) as u64,
        max_alloc: MAX_SINCE_SNAPSHOT.swap(0, Ordering::Relaxed) as u64,
    }
}

/// Read the cumulative alloc/byte counters WITHOUT touching the "biggest
/// single allocation since the last snapshot" window.
///
/// Used for per-system attribution: the benchmark needs per-tick allocs/bytes
/// split by system, but must not reset the per-tick `max_alloc` window (which
/// `snapshot()` owns). Diff two `peek()` results for a region's allocs/bytes;
/// `max_alloc` is 0 by construction and should be ignored from a peek.
pub fn peek() -> AllocSnapshot {
    AllocSnapshot {
        allocs: ALLOCS.load(Ordering::Relaxed) as u64,
        bytes: BYTES.load(Ordering::Relaxed) as u64,
        max_alloc: 0,
    }
}

/// Difference between two snapshots (allocations in the region between them).
#[derive(Clone, Copy, Default)]
pub struct AllocDelta {
    pub allocs: u64,
    pub bytes: u64,
    /// Biggest single allocation (bytes) in the window (from `self`).
    pub max_alloc: u64,
}

impl AllocSnapshot {
    /// Compute the allocation delta between this snapshot and an earlier one.
    ///
    /// `max_alloc` is taken from `self` (the later snapshot): the window's peak
    /// single allocation, since the counter was reset when `base` was taken.
    pub fn diff(&self, base: AllocSnapshot) -> AllocDelta {
        AllocDelta {
            allocs: self.allocs.saturating_sub(base.allocs),
            bytes: self.bytes.saturating_sub(base.bytes),
            max_alloc: self.max_alloc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the per-window "biggest single allocation" tracking:
    /// `snapshot()` captures the window's peak single alloc and resets it, so
    /// `diff(a, b).max_alloc` is the largest allocation made between `a` and `b`.
    ///
    /// BIG is 8MB so it is unique among all allocations the test suite performs
    /// concurrently (the largest other allocation is a ~250KB 500×500 map),
    /// keeping the assertions deterministic despite the shared global allocator.
    #[test]
    fn alloc_window_tracks_peak_and_resets() {
        const BIG: u64 = 8_000_000;
        let a = snapshot();

        // A single large allocation dominates the window; a small one is noise.
        let big = vec![0u8; BIG as usize];
        let _small = vec![0u8; 128];
        let b = snapshot();

        assert!(
            b.max_alloc >= BIG,
            "window max {} should be >= {}",
            b.max_alloc,
            BIG
        );
        assert!(
            b.diff(a).max_alloc >= BIG,
            "diff.max_alloc should carry the window peak"
        );
        // Counters are cumulative and monotonic.
        assert!(b.allocs > a.allocs);
        assert!(b.bytes >= a.bytes + BIG);

        // The next window (b..c) had no 8MB allocation, so the reset took effect.
        drop(big);
        drop(_small);
        let c = snapshot();
        assert!(
            c.max_alloc < BIG,
            "max window should reset each snapshot (got {}, expected < {})",
            c.max_alloc,
            BIG
        );
    }
}
