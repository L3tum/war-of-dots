# War of Dots — Quality & Development Makefile
# Rust axum/WebSocket game server + engine + TypeScript/WebGPU frontend.
# Run with `make` or `make <target>`.

.PHONY: check fmt clippy test miri deny all build-release release-avx2 bench-alloc check-vectorization docs frontend-typecheck frontend-typecheck-e2e frontend-lint frontend-lint-e2e frontend-lint-fix frontend-fmt frontend-fmt-check frontend-check frontend-build frontend-test frontend-e2e e2e deploy-check

# Quick pre-commit checks
check:
	cargo check --all-targets --all-features

fmt:
	cargo fmt --all
	$(MAKE) frontend-fmt

fmt-check:
	cargo fmt --all -- --check
	$(MAKE) frontend-fmt-check

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	# Full suite with the dhat-based alloc_invariants test skipped: dhat uses a
	# process-global allocator, so its net-heap-growth sample is contaminated by
	# whatever other test threads/bins (integration_game, bins) are allocating at
	# the same instant under --all-targets, making the bound flaky. So we run the
	# rest concurrently...
	cargo test --all-targets -- --skip alloc_invariants::warm_tick_net_heap_growth_is_bounded
	# ...then run that one test in isolation (lib target only, single test, no other
	# binary running) where the dhat measurement is clean and the bound is meaningful.
	cargo test --lib alloc_invariants::warm_tick_net_heap_growth_is_bounded -- --exact
	$(MAKE) frontend-test

# MIRI — detects undefined behavior (slow, needs nightly toolchain + miri component)
# Tests use mock data — no real network calls, so isolation is safe
miri:
	cargo +nightly miri test

# cargo-deny — license, security, banned crate checks
deny:
	cargo deny check licenses advisories ban

# Full pre-merge check suite (also rebuilds the frontend bundle so the server
# embeds current source — dist is baked in at compile time via include_dir!)
all: check fmt-check clippy test deny frontend-build frontend-check

# Build for release (static musl binary)
build-release:
	cargo build --release --target x86_64-unknown-linux-musl

# Release build with AVX2 (x86-64-v3): ~29% faster on the influence system,
# but the binary SIGILLs on pre-AVX2 CPUs — only deploy where the host CPU
# has AVX2 (`grep -o avx2 /proc/cpuinfo`, or `make check-vectorization`
# against this build). Default `build-release` stays portable (x86-64 baseline).
release-avx2:
	RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release

# ─── Benchmark with allocation profiling ────────────────────────────────
bench-alloc:
	cargo run --release --features microbenchmarks,bench-alloc -- \
		--benchmark --bench-html benchmark-alloc.html

# ─── Verify hot loops were auto-vectorized ──────────────────────────────
check-vectorization:
	bash scripts/check_vectorization.sh

# Build docs
docs:
	cargo doc --open --no-deps

# ── Frontend targets ──────────────────────────────────────────────────────────
# Run `cd frontend && npm install` once after the first update.

FE := frontend

# TypeScript type checking (no emit)
frontend-typecheck:
	cd $(FE) && npm run typecheck

# E2E harness type check (separate tsconfig — node/ESM Playwright fixtures,
# not the browser bundle)
frontend-typecheck-e2e:
	cd $(FE) && npm run typecheck:e2e

# ESLint — static analysis / code smells
frontend-lint:
	cd $(FE) && npm run lint

# ESLint — the E2E harness (Playwright fixtures + specs), outside the src/ scope
frontend-lint-e2e:
	cd $(FE) && npm run lint:e2e

# ESLint — auto-fix where possible
frontend-lint-fix:
	cd $(FE) && npm run lint:fix

# Prettier — format code
frontend-fmt:
	cd $(FE) && npm run fmt

# Prettier — check formatting without writing
frontend-fmt-check:
	cd $(FE) && npm run fmt:check

# Full frontend quality gate (format check + lint + typecheck)
frontend-check: frontend-fmt-check frontend-lint frontend-typecheck frontend-typecheck-e2e frontend-lint-e2e

# Build the frontend bundle into frontend/dist (served via include_dir and baked
# into the binary at compile time). Run this BEFORE cargo run/build so the server
# embeds the current frontend source.
frontend-build:
	cd $(FE) && npm run build

# Vitest — run frontend unit tests
frontend-test:
	cd $(FE) && npm run test

# No-server WebGPU E2E (Layer 2): builds dist, stages the committed replay
# fixture at dist/replays/, serves it statically, and runs the Playwright suite
# under xvfb-run with the Dawn/SwiftShader software GPU (CPU — no GPU hardware
# needed). Skips with exit 77 (standard "skip" code) when xvfb-run / Chromium /
# @playwright/test are absent. Deliberately NOT in the default `all` path.
frontend-e2e:
	bash $(FE)/e2e/run_e2e.sh

# Convenience alias for the Playwright E2E suite (Layer 2 + live server).
e2e: frontend-e2e

# Deployment docs guard: the operator runbook that documents how to build,
# deploy, and operate a production server (config placeholders, debug
# endpoints off, storage layout) must stay present next to the CI workflow.
deploy-check:
	@test -f .github/DEPLOYMENT.md || (echo "missing .github/DEPLOYMENT.md — create the deployment runbook" && false)
