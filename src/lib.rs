//! War of Dots — library API for game engine and protocol.

#[cfg(all(test, not(feature = "bench-alloc")))]
mod alloc_invariants;
#[cfg(feature = "bench-alloc")]
pub mod bench_alloc;
#[cfg(feature = "microbenchmarks")]
pub mod benchmark;
pub mod config;
pub mod debug;
pub mod errors;
pub mod game;
pub mod llm;
pub mod map;
#[cfg(feature = "microbenchmarks")]
pub mod microbenchmarks;
pub mod server;
pub mod storage;

// Build-config tripwire: proves the committed DEFAULT build stays portable
// (x86-64 baseline, no AVX2). If this fails, someone re-committed a
// target-cpu=x86-64-v3 rustflag (see .cargo/config.toml) and the binary
// would SIGILL on pre-AVX2 CPUs. AVX2 is opt-in per invocation:
// `make release-avx2` / `RUSTFLAGS="-C target-cpu=x86-64-v3"` — a build
// made that way deliberately trips this test (skip it with
// `--skip target_feature_guard` when testing an AVX2 build).
#[cfg(test)]
mod target_feature_guard {
    #[test]
    #[allow(clippy::assertions_on_constants)] // intentional: cfg!() is compile-time; this is a build-config tripwire
    fn x86_64_default_build_is_portable() {
        #[cfg(target_arch = "x86_64")]
        assert!(
            !cfg!(target_feature = "avx2"),
            "default x86_64 build targets AVX2 — the default must stay portable \n\
             (x86-64 baseline). AVX2 is opt-in: make release-avx2 / RUSTFLAGS=\"-C target-cpu=x86-64-v3\""
        );
    }
}
