# War of Dots

A LAN RTS war game where 2-6 players control troops to capture cities, build armies, and expand territory. Built with a single Rust binary serving a WebGPU-powered HTML frontend.

## Features

- **Terrain System**: Plains, Hills (+50% attack, -30% speed), Forests (-25% attack, -20% speed), Water (-50% attack, -40% speed), Mountains (impassable)
- **Fog of War**: Based on troop/city vision radius with hill bonus
- **Territory Borders**: Tracked via troop/city influence
- **Unit System**: Light Infantry, Heavy Infantry, Boat, Tank with soft/hard attack/defense counters
- **Combat**: Terrain modifiers, attrition, surrender mechanics
- **Economy**: Funds from cities, manpower recruitment and healing
- **LLM Agents**: OpenAI, Anthropic, Ollama with configurable thinking delay
- **Advisor**: LLM advisor with strategic suggestions
- **Lobbies**: Create/join with codes, password protection, game settings
- **Replays**: JSONL recording with SQLite metadata
- **Leaderboards**: Per-player aggregate statistics (standard ruleset only)
- **WebGPU Rendering**: Terrain marching squares, unit dots with type symbols, org/health/equipment bars
- **Camera**: Right-click pan, scroll wheel zoom

## Building

### Prerequisites
- Rust 1.70+ (with `cargo`)
- Node.js 18+ (for frontend build)
- WebGPU-capable browser for client

### Build Steps
```bash
# Build frontend
cd frontend
npm install
npm run build
cd ..

# Build Rust server
cargo build --release
```

### Regenerating Frontend Types from Server Protocol

The TypeScript client types in `frontend/src/generated/protocol.ts` are generated from Rust server types using the `schemars` JSON schema pipeline:

```bash
cd frontend
npm run generate-types
```

This runs `cargo run --bin generate_schema` to export Rust protocol types to JSON Schema, then `json-schema-to-typescript` to produce TypeScript definitions. Run this anytime you modify server protocol types (in `src/server/protocol.rs`).

The generated file is automatically included in `.gitignore`, so only the Rust source of truth needs to be committed.

### Running
```bash
# With default config (OpenAI API key required)
OPENAI_API_KEY=sk-xxx cargo run --release -- --port 8080

# With custom config file
cargo run --release -- --config custom_config.toml

# With local LLM (Ollama)
cargo run --release
```

## Configuration

Create a `config.toml` with:

```toml
[llm]
providers = [
  { name = "openai", provider = "openai", model = "gpt-4o", api_key = "${OPENAI_API_KEY}" },
  { name = "ollama", provider = "ollama", model = "llama3", base_url = "http://localhost:11434" },
]

[standard_ruleset]
enabled = true
unit_types = [ ... ]

[storage]
database_path = "war_of_dots.db"
replay_path = "replays"
```

## Game Mechanics

### Unit Types
- **Light Infantry (L)**: 100 men, rifles, radios. Balanced soft combat.
- **Heavy Infantry (H)**: 150 men, anti-tank. High soft/hard defense.
- **Boat (B)**: 50 men, naval movement, machine gun.
- **Tank (T)**: 30 men, high hard attack/defense, high attrition.

### Combat Counters
- Soft attack is strong vs soft defense, weak vs hard defense
- Hard attack is strong vs hard defense, weak vs soft defense
- Terrain modifiers affect attack effectiveness

### Economy
- Cities generate funds based on manpower pool (more manpower = more funds)
- Recruitment costs funds AND manpower
- Healing costs funds and depletes city manpower
- Units start with 0% organisation, building up over time

### Surrender
- Low manpower units have proportional surrender chance
- Encircled units have increased surrender chance
- Surrendered equipment converts to funds for capturer

## Profiling

### Benchmark with Allocation Profiling

Run the full benchmark with the counting allocator enabled to measure per-tick heap allocation:

```bash
make bench-alloc
# or equivalently:
cargo run --release --features microbenchmarks,bench-alloc -- \
    --benchmark --bench-html benchmark-alloc.html
```

The HTML report will include:
- **Allocation Per Tick** section: two line charts (Allocs/Tick, Bytes/Tick) with previous-comparison overlay when `--bench-prev` is given
- **Stack-Resident Scratch Sizes** table: static `size_of` for all major scratch structs
- Per-benchmark `allocs_per_iter` / `bytes_per_iter` in the microbenchmark table

Compare against a previous run:
```bash
make bench-alloc
# then:
cargo run --release --features microbenchmarks,bench-alloc -- \
    --benchmark --bench-html benchmark-alloc.html --bench-prev benchmark-alloc.html
```

### CPU Microarchitecture (perf stat)

For instruction-level profiling (IPC, cache misses, branch mispredicts):

```bash
perf stat cargo run --release -- --benchmark --bench-html /dev/null
```

### Verify Auto-Vectorization

Check whether hot loops were auto-vectorized to AVX2 in the release binary:

```bash
make build-release
make check-vectorization
```

This disassembles the release binary and greps known hot functions (`influence::run`, `nearby_enemies::run`) for SIMD instructions. A positive result is evidence (not per-loop proof); a negative (0 vector ops) IS meaningful. For definitive per-loop verification, use:

```bash
cargo rustc --release -- --emit asm | less
```

### Notes

- `bench-alloc` is incompatible with Miri (replaces the global allocator). Run `make miri` without the feature.
- The counting allocator adds ~1 atomic add per allocation (~0.5 ns overhead). Negligible vs. tick cost.
- Dynamic peak-stack measurement is out of scope; `heaptrack` is the escape hatch if needed.
- The AVX2 build config (`target-cpu=x86-64-v3`) is verified by the `x86_64_build_targets_avx2` unit test on every `cargo test`.

## License

MIT
