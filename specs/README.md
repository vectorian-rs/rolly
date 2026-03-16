# TLA+ Specifications

Formal models proving safety and liveness properties of rolly's concurrency protocols.

## Specifications

| File | What it models | Source |
|------|---------------|--------|
| `exporter.tla` | Bounded MPSC channel, batching, semaphore-bounded exports, flush/shutdown | `src/exporter.rs` |
| `metrics_registry.tla` | RwLock + per-instrument Mutex hierarchy, concurrent reads/writes/collect | `src/metrics.rs` |

## Properties verified

### exporter.tla

- **ChannelBounded** (safety): `Len(channel) <= Capacity` always
- **DataConservation** (safety): `dropped + in_channel + in_batch + in_flight + exported = total_sent`
- **FlushCompletes** (liveness): every Flush message eventually gets a reply
- **ShutdownTerminates** (liveness): after Shutdown, the exporter loop terminates

### metrics_registry.tla

- **RwLockInvariant** (safety): write-lock excludes all readers; readers coexist
- **LockOrdering** (safety): instrument mutexes only held while registry read-lock is held
- **CollectorCompletes** (liveness): the collect() snapshot always finishes
- **WritersComplete** (liveness): all writers eventually finish their updates

## Prerequisites

### Option 1: tla-checker (Rust, recommended)

No Java required. Install from crates.io:

```bash
cargo install tla-checker
```

### Option 2: VS Code extension

Install "TLA+ Nightly" from the VS Code marketplace (requires Java).

### Option 3: tla2tools.jar

```bash
curl -L -o ~/bin/tla2tools.jar \
  https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar
```

## Running the model checker

### With tla-checker (Rust)

```bash
cd specs
tla-checker exporter.tla
tla-checker metrics_registry.tla
```

### With tla2tools.jar (Java)

```bash
cd specs
java -cp ~/bin/tla2tools.jar tlc2.TLC exporter.tla -config exporter.cfg -workers auto
java -cp ~/bin/tla2tools.jar tlc2.TLC metrics_registry.tla -config metrics_registry.cfg -workers auto
```

### With VS Code

1. Open any `.tla` file
2. The TLA+ extension will detect the corresponding `.cfg` file
3. Use **TLA+: Check Model with TLC** from the command palette

## PlusCal translation

The `.tla` files contain PlusCal algorithms (in `(*--algorithm ... *)` blocks) and
their hand-written TLA+ translations. If you modify the PlusCal, re-translate with:

```bash
# With tla2tools.jar:
java -cp ~/bin/tla2tools.jar pcal.trans exporter.tla
java -cp ~/bin/tla2tools.jar pcal.trans metrics_registry.tla
```

## Model parameters

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
