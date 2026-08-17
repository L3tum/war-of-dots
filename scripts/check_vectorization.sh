#!/usr/bin/env bash
# On-demand diagnostic: disassemble the release binary and check whether
# known vectorizable hot functions contain AVX/SIMD instructions.
#
# CAVEATS:
#   - Greps the OUTERMOST symbol (inner #[inline] helpers have no standalone
#     symbol). A positive ("vector ops found") is EVIDENCE but not per-loop
#     proof — other code in the same symbol could emit vector instructions.
#   - A negative (0 vector ops) IS meaningful: the loop almost certainly
#     did not vectorize.
#   - For a definitive per-loop answer, use: cargo rustc --release -- --emit asm
#     and inspect the specific loop region.
#
# Requires an existing target/release/war-of-dots binary (does NOT rebuild
# — build.rs requires node/npm for the frontend).
set -euo pipefail

BIN="${1:-target/release/war-of-dots}"
[[ -f "$BIN" ]] || { echo "error: $BIN not found — run 'cargo build --release' first"; exit 1; }
[[ "$(uname -m)" == "x86_64" ]] || { echo "skip: not x86_64"; exit 0; }

OBJDUMP=$(command -v rust-objdump 2>/dev/null || command -v llvm-objdump 2>/dev/null || command -v objdump 2>/dev/null || true)
if [[ -z "$OBJDUMP" ]]; then
    echo "error: need rust-objdump, llvm-objdump, or objdump (try: rustup component add llvm-tools)"
    exit 1
fi

# Symbols to inspect (outermost, stable in release).
FUNCS=("influence.*run" "nearby_enemies.*run")
# Regex for x86-64-v3 (AVX2) vector ops on ymm registers
AVX_RE='ymm|vcmpps?|vaddps?|vshufp?s?|vpermilpd?|vblendvps?|vpadd|vpcmp|vmaxps?|vminps?'

strict="${2:-}"
fail=0
echo "Checking vectorization in: $BIN"
echo "────────────────────────────────────────────────"

for re in "${FUNCS[@]}"; do
    # Extract disassembly for symbols matching the regex
    body=$("$OBJDUMP" -d "$BIN" 2>/dev/null | awk -v re="$re" '
        /<.*>:/ { if (match($0, re)) { f=1; next } else if (f) { exit } }
        f { print }' || true)
    if [[ -z "$body" ]]; then
        echo "  ⚠ not found: $re (likely inlined or not in this binary)"
        if [[ "$strict" == "--strict" ]]; then
            echo "  FAIL: expected symbol missing"
            fail=1
        fi
        continue
    fi
    n=$(echo "$body" | grep -cE "$AVX_RE" || true)
    if [[ "$n" -gt 0 ]]; then
        echo "  ✓ vector ops found: $re ($n SIMD instructions in symbol body)"
        echo "    NOTE: evidence, not per-loop proof. Check asm for loop-specific patterns for certainty."
    else
        echo "  ✗ NOT vectorized: $re (0 vector ops) — candidate for manual SIMD"
        fail=1
    fi
done

echo "────────────────────────────────────────────────"
[[ "$fail" -ne 0 ]] && { echo "RESULT: FAIL"; exit 1; }
echo "RESULT: PASS"
exit 0
