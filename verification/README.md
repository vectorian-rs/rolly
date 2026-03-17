# Verification

Formal verification, fuzz testing, and property-based testing for rolly.

## Overview

| Tool | What it verifies | Artifacts |
|------|-----------------|-----------|
| TLA+ | Concurrency protocols (channels, locks, flush/shutdown) | `specs/*.tla` |
| Kani | Sequential correctness (protobuf encoding, hashing, sampling) | `src/*.rs` (inline harnesses) |
| cargo-fuzz | Crash/panic freedom under arbitrary inputs | `fuzz/fuzz_targets/*.rs` |
| proptest | Algebraic properties (roundtrip, commutativity, idempotence) | `tests/*.rs` (inline) |

## TLA+ Specifications

Formal models proving safety and liveness properties of rolly's concurrency protocols.

### Specifications

| File | What it models | Source |
|------|---------------|--------|
| `specs/exporter.tla` | Bounded MPSC channel, batching, semaphore-bounded exports, flush/shutdown | `src/exporter.rs` |
| `specs/metrics_registry.tla` | RwLock + per-instrument Mutex hierarchy, concurrent reads/writes/collect | `src/metrics.rs` |

### Properties verified

#### exporter.tla

- **ChannelBounded** (safety): `Len(channel) <= Capacity` always
- **DataConservation** (safety): `dropped + in_channel + in_batch + in_flight + exported = total_sent`
- **FlushCompletes** (liveness): every Flush message eventually gets a reply
- **ShutdownTerminates** (liveness): after Shutdown, the exporter loop terminates

#### metrics_registry.tla

- **RwLockInvariant** (safety): write-lock excludes all readers; readers coexist
- **LockOrdering** (safety): instrument mutexes only held while registry read-lock is held
- **CollectorCompletes** (liveness): the collect() snapshot always finishes
- **WritersComplete** (liveness): all writers eventually finish their updates

### Prerequisites

#### Option 1: tla-checker (Rust, recommended)

No Java required. Install from crates.io:

```bash
cargo install tla-checker
```

#### Option 2: VS Code extension

Install "TLA+ Nightly" from the VS Code marketplace (requires Java).

#### Option 3: tla2tools.jar

```bash
curl -L -o ~/bin/tla2tools.jar \
  https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar
```

### Running the model checker

#### With tla-checker (Rust)

```bash
cd verification/specs
tla-checker exporter.tla
tla-checker metrics_registry.tla
```

#### With tla2tools.jar (Java)

```bash
cd verification/specs
java -cp ~/bin/tla2tools.jar tlc2.TLC exporter.tla -config exporter.cfg -workers auto
java -cp ~/bin/tla2tools.jar tlc2.TLC metrics_registry.tla -config metrics_registry.cfg -workers auto
```

#### With VS Code

1. Open any `.tla` file
2. The TLA+ extension will detect the corresponding `.cfg` file
3. Use **TLA+: Check Model with TLC** from the command palette

### PlusCal translation

The `.tla` files contain PlusCal algorithms (in `(*--algorithm ... *)` blocks) and
their hand-written TLA+ translations. If you modify the PlusCal, re-translate with:

```bash
# With tla2tools.jar:
java -cp ~/bin/tla2tools.jar pcal.trans exporter.tla
java -cp ~/bin/tla2tools.jar pcal.trans metrics_registry.tla
```

### Model parameters

The `.cfg` files use small constants for tractable model checking:

| Parameter | exporter | metrics_registry |
|-----------|----------|-----------------|
| Capacity / - | 2 | - |
| BatchSize / - | 2 | - |
| NumProducers / NumWriters | 2 | 2 |
| MaxConcurrent / - | 2 | - |
| - / NumInstruments | - | 2 |

These small values are sufficient to expose concurrency bugs while keeping
state-space exploration feasible (typically < 1 minute).

## Fuzz Testing

3 fuzz targets using `cargo-fuzz` / `libfuzzer`:

| Target | What it tests |
|--------|--------------|
| `fuzz_encode_trace` | OTLP trace protobuf encoding with arbitrary spans/attributes |
| `fuzz_encode_logs` | OTLP log protobuf encoding with arbitrary log records |
| `fuzz_hex_to_bytes_16` | Hex decoding with arbitrary 32-char inputs |

### Running

```bash
# List targets
cargo fuzz list --fuzz-dir verification/fuzz

# Run a target (runs until interrupted)
cargo fuzz run --fuzz-dir verification/fuzz fuzz_encode_trace

# Run with a time limit
cargo fuzz run --fuzz-dir verification/fuzz fuzz_encode_trace -- -max_total_time=60
```

## Kani Model Checking

19 proof harnesses embedded in source files (`#[cfg(kani)]` blocks). Kani uses bounded model checking (CBMC) to exhaustively verify properties over all possible inputs.

### Running

```bash
# Install Kani
cargo install --locked kani-verifier
cargo kani setup

# Run all proofs
cargo kani

# Run a specific harness
cargo kani --harness proof_encode_varint_no_panic
```

## Property-Based Tests (proptest)

9 proptests embedded in `tests/` files. These generate random inputs and verify algebraic properties (roundtrip encoding, hash commutativity, etc.).

### Running

```bash
cargo test --test proptest
# Or run all tests which includes proptests:
cargo test
```
