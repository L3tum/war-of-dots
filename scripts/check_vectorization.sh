#!/usr/bin/env bash
# On-demand diagnostic: disassemble the release binary and report, per hot
# function, whether wide-register (AVX2) vector *arithmetic* was emitted.
#
# WHY THIS EXISTS
#   .cargo/config.toml sets `-C target-cpu=x86-64-v3` (AVX2) so LLVM
#   auto-vectorizes tight numeric loops. This script is the tripwire: it checks
#   that the loops we EXPECT to vectorize actually did. A function in
#   EXPECT_VECTOR that has 0 vector math is a regression (the loop stopped
#   vectorizing, or the build silently fell back to SSE2).
#
# HOW IT WORKS
#   - Disassembles target/release/war-of-dots with `rust-objdump -d --demangle`.
#     The release binary is NOT stripped (Cargo.toml has no [profile.release]
#     `strip`), so function symbols are retained. `--demangle` maps the mangled
#     name (old `_ZN...` or new `_R...` scheme) to a readable path so we can
#     match `GridIndex::build` / `union_vision_grids` / `influence::run` /
#     `movement::run` / `for_each_within` reliably regardless of mangling.
#   - Per function, counts wide-register vector *arithmetic* ops (not plain
#     loads/stores) via AVX_RE.
#   - EXPECT_VECTOR: MUST show vector math (a miss, or being absent, = FAIL).
#   - REPORT_ONLY: expected-scalar for now (the manual-SIMD backlog) — reported
#     for visibility, never fails.
#
# GUARD AGAINST THE OLD VACUOUS PASS
#   The previous version passed silently when 0 functions matched (it grepped
#   the mangled symbol names without --demangle, so the regexes never matched;
#   and it still listed the deleted `nearby_enemies.*run`). If we now find NONE
#   of the EXPECT_VECTOR symbols, we exit 2 with a diagnostic instead of "PASS".
#
# STALE-BINARY WARNING (not fatal)
#   If any .rs is newer than the binary, the disassembly may not reflect current
#   source — we warn but still run.
#
# PREREQ: a built target/release/war-of-dots (`cargo build --release`).
#        (The script does NOT rebuild — build.rs requires node/npm.)
# Usage:  bash scripts/check_vectorization.sh [binary] [--strict]
#   --strict: also FAIL if a REPORT_ONLY symbol is entirely missing (catches a
#             loop being inlined away that we still expect to exist).
set -euo pipefail

BIN="${1:-target/release/war-of-dots}"
strict="${2:-}"

[[ "$(uname -m)" == "x86_64" ]] || { echo "skip: not x86_64"; exit 0; }
[[ -f "$BIN" ]] || { echo "error: $BIN not found — run 'cargo build --release' first"; exit 1; }

# Staleness warning (non-fatal).
NEWER="$(find src -name '*.rs' -newer "$BIN" 2>/dev/null | head -1 || true)"
if [[ -n "$NEWER" ]]; then
    echo "warn: $BIN is older than $NEWER — rebuild ('cargo build --release') for accurate results"
fi

# Tool selection — the demangler is the crux. rustc >= 1.90 emits the portable
# `_R...` mangling scheme; only rustc's own demangler (bundled in rust-objdump)
# reliably reverses it. Plain binutils objdump CANNOT (its GNU demangler only
# understands C++/old `_ZN`), so we refuse it rather than emit a false "MISSING".
#
# Note: `rustup component add llvm-tools` installs rust-objdump into the
# TOOLCHAIN bin dir (~/.rustup/toolchains/<toolchain>/bin), which is often NOT
# on PATH — so we probe the standard locations, not just $PATH.
# 1) rust-objdump — rustc's wrapper with the Rust demangler (first choice).
OBJDUMP=""
if command -v rust-objdump 2>/dev/null; then
    OBJDUMP="rust-objdump"
else
    # rustup places component tools in <toolchain>/lib/rustlib/<target>/bin
    # (NOT <toolchain>/bin), and only shims the ACTIVE toolchain into
    # ~/.cargo/bin — so probe both layouts across ALL toolchains (a
    # rust-objdump from any toolchain demangles any rustc-built binary).
    for cand in "${CARGO_HOME:-$HOME/.cargo}/bin/rust-objdump" \
                "$HOME"/.rustup/toolchains/*/bin/rust-objdump \
                "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/rust-objdump; do
        if [[ -x "$cand" ]]; then
            OBJDUMP="$cand"
            break
        fi
    done
fi

# 2) llvm-objdump — the raw LLVM tool. Since LLVM 19 (rustc ~1.8x+) its
#    --demangle also handles Rust's new _R mangling, so it is a fine fallback
#    for toolchains whose llvm-tools component ships only the raw llvm-* tools.
#    Same layout caveat, so probe the same places.
if [[ -z "$OBJDUMP" ]]; then
    if command -v llvm-objdump 2>/dev/null; then
        OBJDUMP="llvm-objdump"
    else
        for cand in "${CARGO_HOME:-$HOME/.cargo}/bin/llvm-objdump" \
                    "$HOME"/.rustup/toolchains/*/bin/llvm-objdump \
                    "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-objdump; do
            if [[ -x "$cand" ]]; then
                OBJDUMP="$cand"
                break
            fi
        done
    fi
fi

# 3) Nothing usable. Binutils objdump cannot demangle Rust symbols — say so.
if [[ -z "$OBJDUMP" ]]; then
    if command -v objdump 2>/dev/null; then
        echo "error: only binutils 'objdump' is available, and it cannot demangle Rust"
        echo "       symbols (rustc >= 1.90 uses the portable '_R' mangling scheme,"
        echo "       which binutils' GNU demangler does not understand). That is why the"
        echo "       check reports the symbols as 'MISSING' — the binary is fine; the"
        echo "       tooling is not. Install rustc's LLVM tools once and re-run:"
        echo
        echo "           rustup component add llvm-tools"
        echo
        echo "       (provides rust-objdump / rust-nm) then:  make check-vectorization"
        echo "       If it IS installed but this message still appears, neither"
        echo "       rust-objdump nor llvm-objdump was found under ~/.cargo/bin or"
        echo "       ~/.rustup/toolchains. Locate it with:  find ~/.rustup -name '*objdump*'"
        exit 2
    else
        echo "error: no objdump available (need rust-objdump, or llvm-objdump)."
        echo "       Install with:  rustup component add llvm-tools"
        exit 1
    fi
fi

# Both rust-objdump and llvm-objdump understand --demangle. (llvm-objdump may
# not demangle the newest _R scheme depending on LLVM version — if the EXPECT
# symbols come back MISSING despite this path, install rust-objdump.)
DEMANGLER="--demangle"
OBJDUMP_NAME="$(basename "$OBJDUMP")"
case "$OBJDUMP_NAME" in
    rust-objdump) TOOL_KIND="rust" ;;
    *) TOOL_KIND="other" ;;
esac

# Disassemble once into a temp file (a 14MB binary disassembles to a lot of
# text; avoid holding it in a shell var and re-piping it per function).
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT
if ! "$OBJDUMP" -d $DEMANGLER "$BIN" > "$TMP" 2>/dev/null; then
    echo "error: $OBJDUMP failed on $BIN"
    exit 2
fi
if [[ ! -s "$TMP" ]]; then
    echo "error: disassembly of $BIN is empty — is it a valid x86_64 binary? (rebuild with 'cargo build --release')"
    exit 2
fi

# The verified wide-register (AVX2) auto-vectorization canaries. If any of
# these lose their vector math, the build fell back below x86-64-v3 (a real
# regression).
EXPECT_VECTOR=( 'GridIndex::build' 'Game::union_vision_grid_flat' )
# Expected-scalar (reported, never fails). NOTE: union_vision_grids was
# initially in EXPECT_VECTOR, but it did NOT wide-vectorize when its element
# type was `bool` (1-bit in MIR). Now that it uses `u8` internally it may
# vectorize, but is kept here until a release-build check confirms it.
REPORT_ONLY=( 'Game::union_vision_grids' 'spatial::for_each_within' 'movement::run' 'influence::run' )

# Strict tripwire: a wide-register (ymm/zmm) vector *arithmetic* op on the line.
# Deliberately excludes plain loads/stores (vmov...) and 128-bit (xmm) ops, so a
# scalar or SSE-only loop does NOT register as vectorized. Covers GridIndex::
# build's float math (vdivps/vroundps/vpminsd/vpmaxsd) and vision byte-OR (vpor).
AVX_RE='(vaddps|vsubps|vmulps|vdivps|vminps|vmaxps|vcmpltp?s?|vroundps|vpadd|vpmax|vpmin|vpor|vpblend|vpxor|vpmovz|vinsertf|vmovnt).*%(ymm|zmm)'

# Extract the disassembly body of the first symbol matching the (ERE) name regex.
extract_body() {
    awk -v re="$1" '
        / <.*>:/ {
            if (match($0, re)) { f = 1; next }
            else if (f) { exit }
        }
        f { print }' "$TMP"
}

echo "Checking vectorization in: $BIN   (via $OBJDUMP_NAME $DEMANGLER)"
echo "──────────────────────────────────────────────────────────────────────"

expect_found=0
expect_fail=0
report_missing=0

for re in "${EXPECT_VECTOR[@]}" "${REPORT_ONLY[@]}"; do
    body="$(extract_body "$re" || true)"
    is_expect=0
    if [[ " ${EXPECT_VECTOR[*]} " == *" $re "* ]]; then
        is_expect=1
    fi
    if [[ -z "$body" ]]; then
        if [[ "$is_expect" -eq 1 ]]; then
            echo "  ✗ MISSING (expected vectorized): $re"
            expect_fail=$((expect_fail + 1))
        else
            echo "  · not found (report-only): $re"
            report_missing=$((report_missing + 1))
        fi
        continue
    fi
    if [[ "$is_expect" -eq 1 ]]; then
        expect_found=$((expect_found + 1))
    fi
    n="$(printf '%s\n' "$body" | grep -cE "$AVX_RE" || true)"
    if [[ "$n" -gt 0 ]]; then
        echo "  ✓ vector math: $re  ($n wide-register SIMD ops in symbol body)"
    else
        if [[ "$is_expect" -eq 1 ]]; then
            echo "  ✗ NOT vectorized (expected): $re  — REGRESSION: the loop stopped vectorizing, or the build fell back to SSE2"
            expect_fail=$((expect_fail + 1))
        else
            echo "  – scalar (as expected, backlog): $re  — 0 vector ops; manual-SIMD candidate"
        fi
    fi
done

echo "──────────────────────────────────────────────────────────────────────"

# Guard: if NONE of the expected symbols are present, the disassembly is
# useless (stripped/inlined/wrong binary, OR the demangler couldn't reverse the
# _R scheme) — fail loudly, never "PASS".
if [[ "$expect_found" -eq 0 ]]; then
    echo "RESULT: ERROR — none of the EXPECT_VECTOR symbols were found in $BIN."
    if [[ "$TOOL_KIND" != "rust" ]]; then
        echo "        This machine has only $OBJDUMP_NAME, which may not demangle rustc's"
        echo "        newest '_R' mangling. Install rust-objdump and re-run:"
        echo "            rustup component add llvm-tools"
    else
        echo "        The binary is likely stripped, wrong-arch, or the symbols inlined."
        echo "        Rebuild with 'cargo build --release' and retry (rust-objdump --demangle)."
    fi
    exit 2
fi

# --strict: also fail if a report-only symbol is entirely missing.
if [[ "$strict" == "--strict" && "$report_missing" -gt 0 ]]; then
    echo "RESULT: FAIL — $report_missing report-only symbol(s) missing under --strict"
    exit 1
fi

if [[ "$expect_fail" -ne 0 ]]; then
    echo "RESULT: FAIL — $expect_fail expected vectorized function(s) missing or scalar"
    exit 1
fi

echo "RESULT: PASS — all EXPECT_VECTOR functions show wide-register vector math"
exit 0
