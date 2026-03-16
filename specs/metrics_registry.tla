----------------------- MODULE metrics_registry -----------------------
(*
 * TLA+ specification of the rolly metrics registry locking protocol.
 *
 * Models the two-level lock hierarchy: an outer RwLock on the registry
 * (HashMap of instruments) and per-instrument Mutexes on the data.
 *
 * Corresponds to: src/metrics.rs (MetricsRegistry, Counter, Gauge, Histogram)
 *)

EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    NumWriters,      \* Number of concurrent writer processes
    NumInstruments   \* Number of instrument slots

ASSUME NumWriters \in Nat \ {0}
ASSUME NumInstruments \in Nat \ {0}

Writers    == 1..NumWriters
Instruments == 1..NumInstruments

\* Process IDs:
\*   Writers:   1..NumWriters
\*   Registrar: NumWriters + 1
\*   Collector: NumWriters + 2

RegistrarId == NumWriters + 1
CollectorId == NumWriters + 2

(*--algorithm metrics_registry

variables
    \* RwLock state: "idle", <<"reading", count>>, or "writing"
    registry_lock = "idle",
    reader_count = 0,

    \* Per-instrument mutex: 0 = free, pid = held by that process
    instrument_lock = [i \in Instruments |-> 0],

    \* Per-instrument data value (abstract)
    instrument_data = [i \in Instruments |-> 0],

    \* Number of instruments currently registered
    num_registered = NumInstruments,

    \* Collector's snapshot accumulator
    snapshot = <<>>,

    \* Iteration state for collector
    collect_idx = 0,

    \* How many write rounds each writer performs
    writer_rounds = [w \in Writers |-> 2],

    \* Which instrument each writer targets (cycles through)
    writer_target = [w \in Writers |-> ((w - 1) % NumInstruments) + 1];

\* ---------------------------------------------------------------
\* Writer process: acquire read-lock -> instrument mutex -> update
\*
\* This matches the pattern in Counter::add / Gauge::set:
\*   let counters = self.counters.read().unwrap();
\*   let mut data = counter.inner.data.lock().unwrap();
\* ---------------------------------------------------------------
fair process writer \in Writers
variables
    w_instrument = 0;
begin
W_Loop:
    while writer_rounds[self] > 0 do
        w_instrument := writer_target[self];

        \* Acquire registry read-lock
    W_AcqRead:
        await registry_lock /= "writing";
        reader_count := reader_count + 1;
        registry_lock := "reading";

        \* Acquire instrument mutex
    W_AcqMutex:
        await instrument_lock[w_instrument] = 0;
        instrument_lock[w_instrument] := self;

        \* Update instrument data
    W_Update:
        instrument_data[w_instrument] := instrument_data[w_instrument] + 1;

        \* Release instrument mutex
    W_RelMutex:
        instrument_lock[w_instrument] := 0;

        \* Release registry read-lock
    W_RelRead:
        reader_count := reader_count - 1;
        if reader_count = 0 then
            registry_lock := "idle";
        end if;

        writer_rounds[self] := writer_rounds[self] - 1;
        \* Cycle to next instrument
        writer_target[self] := (w_instrument % NumInstruments) + 1;
    end while;
end process;

\* ---------------------------------------------------------------
\* Registrar process: acquire write-lock -> add instrument
\*
\* This matches the slow path in MetricsRegistry::counter():
\*   let mut counters = self.counters.write().unwrap();
\*   counters.entry(...).or_insert_with(...)
\*
\* In this model all instruments are pre-registered, so this
\* process demonstrates the write-lock exclusion property.
\* ---------------------------------------------------------------
fair process registrar = RegistrarId
begin
R_Start:
    \* Acquire registry write-lock
R_AcqWrite:
    await registry_lock = "idle";
    registry_lock := "writing";

    \* "Register" an instrument (no-op since pre-registered, but
    \* demonstrates exclusive access)
R_Register:
    skip;

    \* Release registry write-lock
R_RelWrite:
    registry_lock := "idle";
end process;

\* ---------------------------------------------------------------
\* Collector process: acquire read-lock -> iterate instruments ->
\*   acquire each mutex -> snapshot -> release
\*
\* This matches MetricsRegistry::collect():
\*   let counters = self.counters.read().unwrap();
\*   for counter in counters.values() {
\*       let mut data = counter.inner.data.lock().unwrap();
\*       ...
\*   }
\* ---------------------------------------------------------------
fair process collector = CollectorId
begin
C_Start:
    snapshot := <<>>;
    collect_idx := 0;

    \* Acquire registry read-lock
C_AcqRead:
    await registry_lock /= "writing";
    reader_count := reader_count + 1;
    registry_lock := "reading";

C_Iterate:
    collect_idx := collect_idx + 1;
    if collect_idx <= num_registered then
        \* Acquire instrument mutex
    C_AcqMutex:
        await instrument_lock[collect_idx] = 0;
        instrument_lock[collect_idx] := CollectorId;

        \* Snapshot the data
    C_Snapshot:
        snapshot := Append(snapshot, instrument_data[collect_idx]);

        \* Release instrument mutex
    C_RelMutex:
        instrument_lock[collect_idx] := 0;

        goto C_Iterate;
    end if;

    \* Release registry read-lock
C_RelRead:
    reader_count := reader_count - 1;
    if reader_count = 0 then
        registry_lock := "idle";
    end if;
end process;

end algorithm; *)

\* BEGIN TRANSLATION
\* ---------------------------------------------------------------
\* The TLA+ translation below is hand-written to match the PlusCal
\* algorithm above. Run pcal.trans metrics_registry.tla to regenerate.
\* ---------------------------------------------------------------

VARIABLES registry_lock, reader_count, instrument_lock, instrument_data,
          num_registered, snapshot, collect_idx, writer_rounds,
          writer_target, w_instrument, pc

vars == << registry_lock, reader_count, instrument_lock, instrument_data,
           num_registered, snapshot, collect_idx, writer_rounds,
           writer_target, w_instrument, pc >>

ProcSet == (Writers) \cup {RegistrarId} \cup {CollectorId}

Init == (* Global variables *)
        /\ registry_lock = "idle"
        /\ reader_count = 0
        /\ instrument_lock = [i \in Instruments |-> 0]
        /\ instrument_data = [i \in Instruments |-> 0]
        /\ num_registered = NumInstruments
        /\ snapshot = <<>>
        /\ collect_idx = 0
        /\ writer_rounds = [w \in Writers |-> 2]
        /\ writer_target = [w \in Writers |-> ((w - 1) % NumInstruments) + 1]
        (* Process writer *)
        /\ w_instrument = [self \in Writers |-> 0]
        /\ pc = [self \in ProcSet |->
                    CASE self \in Writers -> "W_Loop"
                      [] self = RegistrarId -> "R_Start"
                      [] self = CollectorId -> "C_Start"]

\* --- Writer ---

writer_W_Loop(self) ==
    /\ pc[self] = "W_Loop"
    /\ IF writer_rounds[self] > 0
       THEN /\ w_instrument' = [w_instrument EXCEPT ![self] = writer_target[self]]
            /\ pc' = [pc EXCEPT ![self] = "W_AcqRead"]
       ELSE /\ pc' = [pc EXCEPT ![self] = "Done"]
            /\ w_instrument' = w_instrument
    /\ UNCHANGED << registry_lock, reader_count, instrument_lock,
                    instrument_data, num_registered, snapshot, collect_idx,
                    writer_rounds, writer_target >>

writer_W_AcqRead(self) ==
    /\ pc[self] = "W_AcqRead"
    /\ registry_lock /= "writing"
    /\ reader_count' = reader_count + 1
    /\ registry_lock' = "reading"
    /\ pc' = [pc EXCEPT ![self] = "W_AcqMutex"]
    /\ UNCHANGED << instrument_lock, instrument_data, num_registered,
                    snapshot, collect_idx, writer_rounds, writer_target,
                    w_instrument >>

writer_W_AcqMutex(self) ==
    /\ pc[self] = "W_AcqMutex"
    /\ instrument_lock[w_instrument[self]] = 0
    /\ instrument_lock' = [instrument_lock EXCEPT ![w_instrument[self]] = self]
    /\ pc' = [pc EXCEPT ![self] = "W_Update"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_data,
                    num_registered, snapshot, collect_idx, writer_rounds,
                    writer_target, w_instrument >>

writer_W_Update(self) ==
    /\ pc[self] = "W_Update"
    /\ instrument_data' = [instrument_data EXCEPT
                           ![w_instrument[self]] = @ + 1]
    /\ pc' = [pc EXCEPT ![self] = "W_RelMutex"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_lock,
                    num_registered, snapshot, collect_idx, writer_rounds,
                    writer_target, w_instrument >>

writer_W_RelMutex(self) ==
    /\ pc[self] = "W_RelMutex"
    /\ instrument_lock' = [instrument_lock EXCEPT ![w_instrument[self]] = 0]
    /\ pc' = [pc EXCEPT ![self] = "W_RelRead"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_data,
                    num_registered, snapshot, collect_idx, writer_rounds,
                    writer_target, w_instrument >>

writer_W_RelRead(self) ==
    /\ pc[self] = "W_RelRead"
    /\ reader_count' = reader_count - 1
    /\ IF reader_count' = 0
       THEN registry_lock' = "idle"
       ELSE registry_lock' = registry_lock
    /\ writer_rounds' = [writer_rounds EXCEPT ![self] = @ - 1]
    /\ writer_target' = [writer_target EXCEPT
                         ![self] = (w_instrument[self] % NumInstruments) + 1]
    /\ pc' = [pc EXCEPT ![self] = "W_Loop"]
    /\ UNCHANGED << instrument_lock, instrument_data, num_registered,
                    snapshot, collect_idx, w_instrument >>

writer(self) ==
    \/ writer_W_Loop(self)
    \/ writer_W_AcqRead(self)
    \/ writer_W_AcqMutex(self)
    \/ writer_W_Update(self)
    \/ writer_W_RelMutex(self)
    \/ writer_W_RelRead(self)

\* --- Registrar ---

registrar_R_Start ==
    /\ pc[RegistrarId] = "R_Start"
    /\ pc' = [pc EXCEPT ![RegistrarId] = "R_AcqWrite"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_lock,
                    instrument_data, num_registered, snapshot, collect_idx,
                    writer_rounds, writer_target, w_instrument >>

registrar_R_AcqWrite ==
    /\ pc[RegistrarId] = "R_AcqWrite"
    /\ registry_lock = "idle"
    /\ registry_lock' = "writing"
    /\ pc' = [pc EXCEPT ![RegistrarId] = "R_Register"]
    /\ UNCHANGED << reader_count, instrument_lock, instrument_data,
                    num_registered, snapshot, collect_idx, writer_rounds,
                    writer_target, w_instrument >>

registrar_R_Register ==
    /\ pc[RegistrarId] = "R_Register"
    /\ pc' = [pc EXCEPT ![RegistrarId] = "R_RelWrite"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_lock,
                    instrument_data, num_registered, snapshot, collect_idx,
                    writer_rounds, writer_target, w_instrument >>

registrar_R_RelWrite ==
    /\ pc[RegistrarId] = "R_RelWrite"
    /\ registry_lock' = "idle"
    /\ pc' = [pc EXCEPT ![RegistrarId] = "Done"]
    /\ UNCHANGED << reader_count, instrument_lock, instrument_data,
                    num_registered, snapshot, collect_idx, writer_rounds,
                    writer_target, w_instrument >>

registrar ==
    \/ registrar_R_Start
    \/ registrar_R_AcqWrite
    \/ registrar_R_Register
    \/ registrar_R_RelWrite

\* --- Collector ---

collector_C_Start ==
    /\ pc[CollectorId] = "C_Start"
    /\ snapshot' = <<>>
    /\ collect_idx' = 0
    /\ pc' = [pc EXCEPT ![CollectorId] = "C_AcqRead"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_lock,
                    instrument_data, num_registered, writer_rounds,
                    writer_target, w_instrument >>

collector_C_AcqRead ==
    /\ pc[CollectorId] = "C_AcqRead"
    /\ registry_lock /= "writing"
    /\ reader_count' = reader_count + 1
    /\ registry_lock' = "reading"
    /\ pc' = [pc EXCEPT ![CollectorId] = "C_Iterate"]
    /\ UNCHANGED << instrument_lock, instrument_data, num_registered,
                    snapshot, collect_idx, writer_rounds, writer_target,
                    w_instrument >>

collector_C_Iterate ==
    /\ pc[CollectorId] = "C_Iterate"
    /\ collect_idx' = collect_idx + 1
    /\ IF collect_idx' <= num_registered
       THEN pc' = [pc EXCEPT ![CollectorId] = "C_AcqMutex"]
       ELSE pc' = [pc EXCEPT ![CollectorId] = "C_RelRead"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_lock,
                    instrument_data, num_registered, snapshot,
                    writer_rounds, writer_target, w_instrument >>

collector_C_AcqMutex ==
    /\ pc[CollectorId] = "C_AcqMutex"
    /\ instrument_lock[collect_idx] = 0
    /\ instrument_lock' = [instrument_lock EXCEPT ![collect_idx] = CollectorId]
    /\ pc' = [pc EXCEPT ![CollectorId] = "C_Snapshot"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_data,
                    num_registered, snapshot, collect_idx, writer_rounds,
                    writer_target, w_instrument >>

collector_C_Snapshot ==
    /\ pc[CollectorId] = "C_Snapshot"
    /\ snapshot' = Append(snapshot, instrument_data[collect_idx])
    /\ pc' = [pc EXCEPT ![CollectorId] = "C_RelMutex"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_lock,
                    instrument_data, num_registered, collect_idx,
                    writer_rounds, writer_target, w_instrument >>

collector_C_RelMutex ==
    /\ pc[CollectorId] = "C_RelMutex"
    /\ instrument_lock' = [instrument_lock EXCEPT ![collect_idx] = 0]
    /\ pc' = [pc EXCEPT ![CollectorId] = "C_Iterate"]
    /\ UNCHANGED << registry_lock, reader_count, instrument_data,
                    num_registered, snapshot, collect_idx, writer_rounds,
                    writer_target, w_instrument >>

collector_C_RelRead ==
    /\ pc[CollectorId] = "C_RelRead"
    /\ reader_count' = reader_count - 1
    /\ IF reader_count' = 0
       THEN registry_lock' = "idle"
       ELSE registry_lock' = registry_lock
    /\ pc' = [pc EXCEPT ![CollectorId] = "Done"]
    /\ UNCHANGED << instrument_lock, instrument_data, num_registered,
                    snapshot, collect_idx, writer_rounds, writer_target,
                    w_instrument >>

collector_proc ==
    \/ collector_C_Start
    \/ collector_C_AcqRead
    \/ collector_C_Iterate
    \/ collector_C_AcqMutex
    \/ collector_C_Snapshot
    \/ collector_C_RelMutex
    \/ collector_C_RelRead

\* Complete Next-state relation
Next ==
    \/ (\E self \in Writers: writer(self))
    \/ registrar
    \/ collector_proc

Spec == /\ Init
        /\ [][Next]_vars
        /\ \A self \in Writers : WF_vars(writer(self))
        /\ WF_vars(registrar)
        /\ WF_vars(collector_proc)

\* END TRANSLATION

\* ===============================================================
\* Safety Properties
\* ===============================================================

\* 1. Mutual exclusion: no two processes hold the same instrument mutex
MutualExclusion ==
    \A i \in Instruments :
        instrument_lock[i] /= 0 =>
            ~\E p1, p2 \in ProcSet :
                /\ p1 /= p2
                /\ instrument_lock[i] = p1
                /\ instrument_lock[i] = p2

\* Stronger: at most one holder per instrument
InstrumentMutexExclusive ==
    \A i \in Instruments :
        \A p1, p2 \in ProcSet :
            (instrument_lock[i] = p1 /\ instrument_lock[i] = p2) => p1 = p2

\* 2. RwLock invariant: writer excludes all readers; multiple readers OK
RwLockInvariant ==
    /\ (registry_lock = "writing" => reader_count = 0)
    /\ (reader_count > 0 => registry_lock = "reading")
    /\ (registry_lock = "idle" => reader_count = 0)
    /\ reader_count >= 0

\* 3. No process holds an instrument lock without holding the registry lock
\*    (lock ordering: registry first, then instrument)
LockOrdering ==
    \A p \in Writers :
        (\E i \in Instruments : instrument_lock[i] = p) =>
            (registry_lock = "reading" /\ reader_count > 0)

CollectorLockOrdering ==
    (\E i \in Instruments : instrument_lock[i] = CollectorId) =>
        (registry_lock = "reading" /\ reader_count > 0)

\* ===============================================================
\* Liveness Properties (require fairness)
\* ===============================================================

\* 4. Collector eventually finishes its snapshot
CollectorCompletes ==
    (pc[CollectorId] = "C_Start") ~> (pc[CollectorId] = "Done")

\* 5. All writers eventually finish
WritersComplete ==
    \A w \in Writers :
        (pc[w] = "W_Loop") ~> (pc[w] = "Done")

\* 6. Deadlock freedom
AllTerminated ==
    \A self \in ProcSet : pc[self] = "Done"

DeadlockFreedom == [](~AllTerminated => ENABLED(Next))

=======================================================================
