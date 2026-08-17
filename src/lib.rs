//! War of Dots — library API for game engine and protocol.

#[cfg(all(test, not(feature = "bench-alloc")))]
mod alloc_invariants;
#[cfg(feature = "bench-alloc")]
pub mod bench_alloc;
pub mod benchmark;
pub mod config;
pub mod debug;
pub mod errors;
pub mod frontend;
pub mod game;
pub mod llm;
pub mod map;
#[cfg(feature = "microbenchmarks")]
pub mod microbenchmarks;
pub mod replay;
pub mod server;
pub mod storage;

// Build-config tripwire: proves the x86-64 release target has AVX2 enabled
// via .cargo/config.toml rustflags. If this fails, the binary silently falls
// back to SSE2 and the influence system loses ~29% throughput.
#[cfg(test)]
mod target_feature_guard {
    #[test]
    #[allow(clippy::assertions_on_constants)] // intentional: cfg!() is compile-time; this is a build-config tripwire
    fn x86_64_build_targets_avx2() {
        #[cfg(target_arch = "x86_64")]
        assert!(
            cfg!(target_feature = "avx2"),
            "x86_64 build lacks AVX2 — check .cargo/config.toml rustflags / target-cpu"
        );
    }
}
