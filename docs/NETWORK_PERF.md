# Network Perf — Deferred Items

From `plans/deep-plan-1788916975781-789f68.md` (network-layer performance round). These items were deferred with explicit re-entry triggers — revisit only when the trigger fires.

## Deferral triggers

- **I9 (unit AOI)** — trigger: wire still > ~5 MB/tick after I4, or lobby scale grows past benchmark.
- **I10 (off-tick encode worker)** — trigger: `net_ms` P90 back above tick budget with no wire win left.
- **I3b (numeric unit id)** — trigger: `unit_updates` bytes still a top wire contributor after I4/I3.

## Baseline moved (post-implementation A/B)

All remaining items landed; the round-3 baseline (`stress_i5.html`: 13.43 ms/tick net, 89.6k allocs, 9.86 MB wire on movement·grid-dirty) is obsolete — compare future A/Bs against `reports/stress_i3.html` (latest in the round):

- **I6** — zero-temp grid splice (no per-player temp `Vec` in splice path)
- **I1** — incremental baseline-keyed snapshot (entity-indexed baseline store, no per-tick `UnitState` materialization)
- **I4** — sparse grid diffs (packed changed-cell lists with full-grid fallback)
- **I3** — compact unit updates (equipment counts-only; quantized position)
- **I7** — bounded keep-latest egress (`Delta`/`Reliable` envelope, keep-latest ring, no unbounded growth)
