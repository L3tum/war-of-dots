# CBOR Encoding Compatibility: cbor-x (JS) ↔ ciborium (Rust)

## Problem

The frontend uses `cbor-x` (JS) and the server uses a Rust CBOR library. Two independent encoding incompatibilities were discovered when the browser's CreateLobby messages failed to decode with `"invalid type: map, expected variant identifier"`.

### Issue 1: B9 Map Encoding (enum variant deserialization)

`cbor-x` encodes all maps using the `B9` wire format (major type 5, additional info 25 = uint16 length follows), regardless of map size:

```
B9 00 01 6c 4c6f6262794d657373616765  →  map{1}: "LobbyMessage"
```

`serde_cbor` (0.11) and its fork `serde_cbor_2` (0.13) cannot decode `B9`-encoded maps when deserializing into Rust enum variants. Plain structs work fine; only enum variants fail.

### Issue 2: Integer Encoding for Whole-Number Floats

`cbor-x` encodes JavaScript numbers that are whole numbers as CBOR integers, not floats:

```
llm_thinking_delay: 3.0  →  CBOR 0x03 (uint8), not 0xFA 40 09 00 00 (float32)
```

Both `serde_cbor` and `ciborium` reject CBOR integers when the target Rust field is `f32` or `f64`.

## Library Comparison

| Library | Version | Last release | B9 enum decode | Integer→float | Active? |
|---------|---------|--------------|----------------|---------------|---------|
| `serde_cbor` | 0.11.2 | Aug 2021 | ❌ | ❌ | No |
| `serde_cbor_2` (kanidm) | 0.13.0 | Jun 2025 | ❌ | ❌ | Yes |
| `ciborium` (enarx) | 0.2.2 | Jan 2024 | ✅ | ❌ (strict) | Yes |

**Decision: `ciborium`** — actively maintained, handles B9 correctly, and the integer→float issue is fixable with a targeted deserializer.

## Implementation

### Server: `src/server/protocol.rs`

**Decode function** — direct ciborium decode, no JSON intermediary:

```rust
pub fn decode_client_message(data: &[u8]) -> anyhow::Result<ClientMessage> {
    let msg: ClientMessage = ciborium::from_reader(data)?;
    Ok(msg)
}
```

**Float deserializer** — accepts CBOR integers, floats, and signed integers for `f32`/`f64` fields:

```rust
mod float_deser {
    use serde::{self, Deserialize, Deserializer};

    pub fn deserialize_f32<'de, D>(deserializer: D) -> Result<f32, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Number { F32(f32), F64(f64), I64(i64), U64(u64) }
        match Number::deserialize(deserializer)? {
            Number::F32(v) => Ok(v),
            Number::F64(v) => Ok(v as f32),
            Number::I64(v) => Ok(v as f32),
            Number::U64(v) => Ok(v as f32),
        }
    }

    pub fn deserialize_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Number { F64(f64), I64(i64), U64(u64) }
        match Number::deserialize(deserializer)? {
            Number::F64(v) => Ok(v),
            Number::I64(v) => Ok(v as f64),
            Number::U64(v) => Ok(v as f64),
        }
    }
}
```

**Applied fields** in `LobbySettings`:

| Field | Type | Deserializer |
|-------|------|-------------|
| `funds_per_city` | `f32` | `float_deser::deserialize_f32` |
| `game_speed` | `f32` | `float_deser::deserialize_f32` |
| `llm_thinking_delay` | `f64` | `float_deser::deserialize_f64` |
| `recruitment_rate` | `f32` | `float_deser::deserialize_f32` |
| `water_speed` | `f32` | `float_deser::deserialize_f32` |

### Cargo.toml

```toml
ciborium = "0.2"
serde_cbor = "0.11"   # retained for test helpers that produce compact CBOR
```

### Tests

- `lobby_message_browser_hex_roundtrip` — decodes the exact CBOR bytes captured from the browser network tab (B9 maps + integer `llm_thinking_delay: 3`)
- `lobby_message_minimal_standard_ruleset_decode` — verifies minimal payload with `#[serde(default)]` fills in missing fields

## Why Not the Two-Step Decode?

The CBOR→JSON→ClientMessage approach works but adds a JSON serialization round-trip (~10-50μs/message) and an extra dependency path. The ciborium + float_deser approach:

- Direct CBOR decode, no intermediary format
- No JSON dependency for message decoding
- Explicit about which fields need lenient coercion
- ciborium is actively maintained (enarx/Richard Zak, commits through 2026)
