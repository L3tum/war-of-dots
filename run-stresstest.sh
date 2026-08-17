#!/bin/bash

# Stress-test the benchmark with a large initial army on a large map.
#
# Usage: ./run-stresstest.sh <report-suffix> [extra cargo args]
#
# Defaults (applied only when the matching flag is absent):
#   --bench-initial-units 10000   total units across all cities
#   --bench-map-size 1000         1000×1000 map (spreads units, cuts combat density)
# Any other args (e.g. --bench-ticks, --bench-seed) are forwarded as-is.

SUFFIX="${1:?usage: run-stresstest.sh <report-suffix> [extra args]}"
shift
EXTRA=("$@")

# Apply a default only if the user hasn't already set that flag.
has_flag() {
    local want="$1" a; shift
    for a in "$@"; do
        case "$a" in
            "$want" | "${want}="*) return 0 ;;
        esac
    done
    return 1
}

has_flag --bench-initial-units "${EXTRA[@]}" || EXTRA+=(--bench-initial-units 10000)
has_flag --bench-map-size "${EXTRA[@]}" || EXTRA+=(--bench-map-size 1000)

cargo run --profile release --features debug-overlay,microbenchmarks,bench-alloc -- \
    --benchmark --bench-seed 42 --bench-ticks 1500 \
    --bench-html "./reports/stress_${SUFFIX}.html" "${EXTRA[@]}"
