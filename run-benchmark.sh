#!/bin/bash

cargo run --profile release --features debug-overlay,microbenchmarks,bench-alloc -- --benchmark --bench-seed 42 --bench-ticks 1500 --bench-html "./reports/report_${1}.html" "${2}"
