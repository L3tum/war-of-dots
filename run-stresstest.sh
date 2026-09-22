#!/bin/bash

# Stress-test the benchmark with a large initial army on a large map.
#
# Usage: ./run-stresstest.sh <report-suffix> [extra cargo args]
#
# Defaults (applied only when the matching flag is absent):
#   --bench-initial-units 10000   total units across all cities
#   --bench-map-size 1000         1000×1000 map (spreads units, cuts combat density)
#   --bench-cities 100            10×10 city lattice (realistic city:unit ratio; the
#                                 authored 12-city layout is a poor fit for a 10k army)
#   --bench-ticks 1500            tick count (pass --bench-ticks N for a short smoke run)
#   --bench-splice                splice encoder (production parity: game_server always
#                                 runs the splice path, so the A/B harness must too)
# Any other args (e.g. --bench-seed) are forwarded as-is.

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
has_flag --bench-cities "${EXTRA[@]}" || EXTRA+=(--bench-cities 100)
has_flag --bench-ticks "${EXTRA[@]}" || EXTRA+=(--bench-ticks 1500)
has_flag --bench-splice "${EXTRA[@]}" || EXTRA+=(--bench-splice)

mkdir -p reports
cargo run --profile release --features debug-overlay,microbenchmarks,bench-alloc -- \
    --benchmark --bench-seed 42 \
    --bench-html "./reports/stress_${SUFFIX}.html" "${EXTRA[@]}"
