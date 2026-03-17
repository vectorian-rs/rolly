--------------------------- MODULE exporter ---------------------------
(*
 * TLA+ specification of the rolly exporter channel protocol.
 *
 * Models the message flow from producers through a bounded MPSC channel
 * to the exporter loop, which batches items and dispatches concurrent
 * exports bounded by a semaphore.
 *
 * Corresponds to: src/exporter.rs
 *)

EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Capacity,        \* Bounded channel capacity
    BatchSize,       \* Flush when batch reaches this size
    NumProducers,    \* Number of concurrent producer processes
    MaxConcurrent    \* Semaphore bound on in-flight exports

ASSUME Capacity \in Nat \ {0}
ASSUME BatchSize \in Nat \ {0}
ASSUME NumProducers \in Nat \ {0}
ASSUME MaxConcurrent \in Nat \ {0}

Producers == 1..NumProducers

\* Message types matching ExportMessage enum
MsgData    == "Data"
MsgFlush   == "Flush"
MsgShut    == "Shutdown"

(*--algorithm exporter

variables
    \* Bounded MPSC channel (sequence, max length = Capacity)
    channel = <<>>,

    \* Exporter's accumulated batch
    batch = <<>>,

    \* Number of in-flight export tasks (bounded by MaxConcurrent)
    in_flight = 0,

    \* Total items successfully exported
    exported = 0,

    \* Total items dropped due to full channel
    dropped = 0,

    \* Total items sent by all producers (Data messages attempted)
    total_sent = 0,

    \* Flush reply channels: set of producer IDs awaiting reply
    flush_pending = {},

    \* Whether the exporter loop has terminated
    terminated = FALSE,

    \* Whether shutdown has been sent
    shutdown_sent = FALSE,

    \* Per-producer: how many data messages to send before flush/shutdown
    \* Each producer sends 2 Data, then 1 Flush, then is done.
    \* The last producer also sends Shutdown after its flush completes.
    producer_phase = [p \in Producers |-> "data1"],

    \* Tick flag: models the interval timer firing
    tick_fired = FALSE;

\* ---------------------------------------------------------------
\* Macro: try to enqueue a message (non-blocking, like try_send)
\* ---------------------------------------------------------------
macro try_send(msg, success) begin
    if Len(channel) < Capacity then
        channel := Append(channel, msg);
        success := TRUE;
    else
        success := FALSE;
    end if;
end macro;

\* ---------------------------------------------------------------
\* Producer process: sends Data, Flush messages via try_send
\* ---------------------------------------------------------------
fair process producer \in Producers
variables
    send_ok = FALSE;
begin
P_Loop:
    while producer_phase[self] /= "done" do
        if producer_phase[self] = "data1" then
            \* First data message
            total_sent := total_sent + 1;
            try_send(MsgData, send_ok);
            if ~send_ok then
                dropped := dropped + 1;
            end if;
            producer_phase[self] := "data2";

        elsif producer_phase[self] = "data2" then
            \* Second data message
            total_sent := total_sent + 1;
            try_send(MsgData, send_ok);
            if ~send_ok then
                dropped := dropped + 1;
            end if;
            producer_phase[self] := "flush";

        elsif producer_phase[self] = "flush" then
            \* Flush uses blocking send (await space)
            await Len(channel) < Capacity;
            channel := Append(channel, MsgFlush);
            producer_phase[self] := "wait_flush";

        elsif producer_phase[self] = "wait_flush" then
            \* Wait for flush reply
            await self \notin flush_pending;
            if self = NumProducers then
                \* Last producer sends shutdown
                producer_phase[self] := "shutdown";
            else
                producer_phase[self] := "done";
            end if;

        elsif producer_phase[self] = "shutdown" then
            \* Shutdown uses blocking send
            await Len(channel) < Capacity;
            channel := Append(channel, MsgShut);
            shutdown_sent := TRUE;
            producer_phase[self] := "done";
        end if;
    end while;
end process;

\* ---------------------------------------------------------------
\* Timer process: periodically sets tick_fired
\* ---------------------------------------------------------------
fair process timer = 0
begin
T_Loop:
    while ~terminated do
        tick_fired := TRUE;
    T_Wait:
        await ~tick_fired \/ terminated;
    end while;
end process;

\* ---------------------------------------------------------------
\* Export completion process: models async task completion
\* (each in-flight export eventually completes)
\* ---------------------------------------------------------------
fair process completer = NumProducers + 1
begin
C_Loop:
    while ~terminated \/ in_flight > 0 do
        await in_flight > 0;
        \* One export task completes
        exported := exported + 1;
        in_flight := in_flight - 1;
    end while;
end process;

\* ---------------------------------------------------------------
\* Exporter loop: biased select — channel first, then tick
\* ---------------------------------------------------------------
fair process exporter_loop = NumProducers + 2
variables
    msg = "none",
    flush_requester = 0;
begin
E_Loop:
    while ~terminated do
        \* Biased select: prefer channel over tick
        if Len(channel) > 0 then
            \* Receive from channel
            msg := Head(channel);
            channel := Tail(channel);

            if msg = MsgData then
                batch := Append(batch, MsgData);
                \* Check if batch is full
                if Len(batch) >= BatchSize then
                    goto E_FlushBatch;
                end if;

            elsif msg = MsgFlush then
                \* Record which producer is waiting for flush reply
                \* We use a counter to track flush requesters
                with p \in {p2 \in Producers : producer_phase[p2] = "wait_flush" /\ p2 \notin flush_pending} do
                    flush_requester := p;
                    flush_pending := flush_pending \union {p};
                end with;
                goto E_HandleFlush;

            elsif msg = MsgShut then
                goto E_HandleShutdown;
            end if;

        elsif tick_fired then
            \* Timer tick: flush whatever is in the batch
            tick_fired := FALSE;
            if Len(batch) > 0 then
                goto E_FlushBatch;
            end if;
        end if;
        goto E_Loop;

    \* ---- Flush batch to in-flight exports ----
    E_FlushBatch:
        \* Wait for semaphore permit (in_flight < MaxConcurrent)
        await in_flight < MaxConcurrent;
        in_flight := in_flight + Len(batch);
        batch := <<>>;
        goto E_Loop;

    \* ---- Handle Flush message ----
    E_HandleFlush:
        \* Flush remaining batch
        if Len(batch) > 0 then
            await in_flight < MaxConcurrent;
            in_flight := in_flight + Len(batch);
            batch := <<>>;
        end if;
    E_FlushDrain:
        \* Drain any remaining channel messages before waiting
        if Len(channel) > 0 /\ Head(channel) = MsgData then
            batch := Append(batch, MsgData);
            channel := Tail(channel);
            goto E_FlushDrain;
        end if;
        \* Flush any drained items
        if Len(batch) > 0 then
            await in_flight < MaxConcurrent;
            in_flight := in_flight + Len(batch);
            batch := <<>>;
        end if;
    E_WaitInFlight:
        \* Wait for all in-flight exports to complete
        await in_flight = 0;
        \* Reply on oneshot
        flush_pending := flush_pending \ {flush_requester};
        flush_requester := 0;
        goto E_Loop;

    \* ---- Handle Shutdown ----
    E_HandleShutdown:
        \* Flush remaining batch
        if Len(batch) > 0 then
            await in_flight < MaxConcurrent;
            in_flight := in_flight + Len(batch);
            batch := <<>>;
        end if;
    E_ShutdownDrain:
        \* Drain remaining channel messages
        if Len(channel) > 0 then
            msg := Head(channel);
            channel := Tail(channel);
            if msg = MsgData then
                batch := Append(batch, MsgData);
            end if;
            goto E_ShutdownDrain;
        end if;
        \* Flush drained items
        if Len(batch) > 0 then
            await in_flight < MaxConcurrent;
            in_flight := in_flight + Len(batch);
            batch := <<>>;
        end if;
    E_ShutdownWait:
        \* Wait for all in-flight exports
        await in_flight = 0;
        terminated := TRUE;
    end while;
end process;

end algorithm; *)

\* BEGIN TRANSLATION  (this is generated by the PlusCal translator)
\* ---------------------------------------------------------------
\* The TLA+ translation is generated by running the PlusCal
\* translator. The section below is a placeholder; run:
\*   pcal.trans exporter.tla
\* to produce the actual TLA+ translation.
\*
\* For model checking with TLC, the PlusCal algorithm above is
\* the authoritative specification.
\* ---------------------------------------------------------------

VARIABLES channel, batch, in_flight, exported, dropped, total_sent,
          flush_pending, terminated, shutdown_sent, producer_phase,
          tick_fired, send_ok, msg, flush_requester, pc

vars == << channel, batch, in_flight, exported, dropped, total_sent,
           flush_pending, terminated, shutdown_sent, producer_phase,
           tick_fired, send_ok, msg, flush_requester, pc >>

ProcSet == (Producers) \cup {0} \cup {NumProducers + 1} \cup {NumProducers + 2}

Init == (* Global variables *)
        /\ channel = <<>>
        /\ batch = <<>>
        /\ in_flight = 0
        /\ exported = 0
        /\ dropped = 0
        /\ total_sent = 0
        /\ flush_pending = {}
        /\ terminated = FALSE
        /\ shutdown_sent = FALSE
        /\ producer_phase = [p \in Producers |-> "data1"]
        /\ tick_fired = FALSE
        (* Process producer *)
        /\ send_ok = [self \in Producers |-> FALSE]
        (* Process exporter_loop *)
        /\ msg = "none"
        /\ flush_requester = 0
        /\ pc = [self \in ProcSet |->
                    CASE self \in Producers -> "P_Loop"
                      [] self = 0 -> "T_Loop"
                      [] self = NumProducers + 1 -> "C_Loop"
                      [] self = NumProducers + 2 -> "E_Loop"]

\* Producer process
producer(self) ==
    /\ pc[self] = "P_Loop"
    /\ IF producer_phase[self] /= "done"
       THEN
            IF producer_phase[self] = "data1"
            THEN
                /\ total_sent' = total_sent + 1
                /\ IF Len(channel) < Capacity
                   THEN /\ channel' = Append(channel, MsgData)
                        /\ send_ok' = [send_ok EXCEPT ![self] = TRUE]
                   ELSE /\ send_ok' = [send_ok EXCEPT ![self] = FALSE]
                        /\ channel' = channel
                /\ IF ~send_ok'[self]
                   THEN /\ dropped' = dropped + 1
                   ELSE /\ dropped' = dropped
                /\ producer_phase' = [producer_phase EXCEPT ![self] = "data2"]
                /\ pc' = [pc EXCEPT ![self] = "P_Loop"]
                /\ UNCHANGED << flush_pending, shutdown_sent >>
            ELSE IF producer_phase[self] = "data2"
            THEN
                /\ total_sent' = total_sent + 1
                /\ IF Len(channel) < Capacity
                   THEN /\ channel' = Append(channel, MsgData)
                        /\ send_ok' = [send_ok EXCEPT ![self] = TRUE]
                   ELSE /\ send_ok' = [send_ok EXCEPT ![self] = FALSE]
                        /\ channel' = channel
                /\ IF ~send_ok'[self]
                   THEN /\ dropped' = dropped + 1
                   ELSE /\ dropped' = dropped
                /\ producer_phase' = [producer_phase EXCEPT ![self] = "flush"]
                /\ pc' = [pc EXCEPT ![self] = "P_Loop"]
                /\ UNCHANGED << flush_pending, shutdown_sent >>
            ELSE IF producer_phase[self] = "flush"
            THEN
                /\ Len(channel) < Capacity
                /\ channel' = Append(channel, MsgFlush)
                /\ producer_phase' = [producer_phase EXCEPT ![self] = "wait_flush"]
                /\ pc' = [pc EXCEPT ![self] = "P_Loop"]
                /\ UNCHANGED << total_sent, dropped, send_ok, flush_pending, shutdown_sent >>
            ELSE IF producer_phase[self] = "wait_flush"
            THEN
                /\ self \notin flush_pending
                /\ IF self = NumProducers
                   THEN /\ producer_phase' = [producer_phase EXCEPT ![self] = "shutdown"]
                   ELSE /\ producer_phase' = [producer_phase EXCEPT ![self] = "done"]
                /\ pc' = [pc EXCEPT ![self] = "P_Loop"]
                /\ UNCHANGED << channel, total_sent, dropped, send_ok, flush_pending, shutdown_sent >>
            ELSE IF producer_phase[self] = "shutdown"
            THEN
                /\ Len(channel) < Capacity
                /\ channel' = Append(channel, MsgShut)
                /\ shutdown_sent' = TRUE
                /\ producer_phase' = [producer_phase EXCEPT ![self] = "done"]
                /\ pc' = [pc EXCEPT ![self] = "P_Loop"]
                /\ UNCHANGED << total_sent, dropped, send_ok, flush_pending >>
            ELSE /\ pc' = [pc EXCEPT ![self] = "P_Loop"]
                 /\ UNCHANGED << channel, total_sent, dropped, send_ok,
                                 flush_pending, shutdown_sent, producer_phase >>
       ELSE /\ pc' = [pc EXCEPT ![self] = "Done"]
            /\ UNCHANGED << channel, total_sent, dropped, send_ok,
                            flush_pending, shutdown_sent, producer_phase >>
    /\ UNCHANGED << batch, in_flight, exported, terminated, tick_fired,
                    msg, flush_requester >>

\* Timer process
timer_T_Loop ==
    /\ pc[0] = "T_Loop"
    /\ IF ~terminated
       THEN /\ tick_fired' = TRUE
            /\ pc' = [pc EXCEPT ![0] = "T_Wait"]
       ELSE /\ pc' = [pc EXCEPT ![0] = "Done"]
            /\ tick_fired' = tick_fired
    /\ UNCHANGED << channel, batch, in_flight, exported, dropped, total_sent,
                    flush_pending, terminated, shutdown_sent, producer_phase,
                    send_ok, msg, flush_requester >>

timer_T_Wait ==
    /\ pc[0] = "T_Wait"
    /\ (~tick_fired \/ terminated)
    /\ pc' = [pc EXCEPT ![0] = "T_Loop"]
    /\ UNCHANGED << channel, batch, in_flight, exported, dropped, total_sent,
                    flush_pending, terminated, shutdown_sent, producer_phase,
                    tick_fired, send_ok, msg, flush_requester >>

timer == timer_T_Loop \/ timer_T_Wait

\* Completer process
completer ==
    /\ pc[NumProducers + 1] = "C_Loop"
    /\ IF ~terminated \/ in_flight > 0
       THEN /\ in_flight > 0
            /\ exported' = exported + 1
            /\ in_flight' = in_flight - 1
            /\ pc' = [pc EXCEPT ![NumProducers + 1] = "C_Loop"]
       ELSE /\ pc' = [pc EXCEPT ![NumProducers + 1] = "Done"]
            /\ UNCHANGED << in_flight, exported >>
    /\ UNCHANGED << channel, batch, dropped, total_sent, flush_pending,
                    terminated, shutdown_sent, producer_phase, tick_fired,
                    send_ok, msg, flush_requester >>

\* Exporter loop
exporter_E_Loop ==
    /\ pc[NumProducers + 2] = "E_Loop"
    /\ IF ~terminated
       THEN IF Len(channel) > 0
            THEN /\ msg' = Head(channel)
                 /\ channel' = Tail(channel)
                 /\ IF msg' = MsgData
                    THEN /\ batch' = Append(batch, MsgData)
                         /\ IF Len(batch') >= BatchSize
                            THEN /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_FlushBatch"]
                            ELSE /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_Loop"]
                         /\ UNCHANGED << flush_pending, flush_requester >>
                    ELSE IF msg' = MsgFlush
                    THEN /\ \E p \in {p2 \in Producers : producer_phase[p2] = "wait_flush" /\ p2 \notin flush_pending} :
                              /\ flush_requester' = p
                              /\ flush_pending' = flush_pending \union {p}
                         /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_HandleFlush"]
                         /\ batch' = batch
                    ELSE IF msg' = MsgShut
                    THEN /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_HandleShutdown"]
                         /\ UNCHANGED << batch, flush_pending, flush_requester >>
                    ELSE /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_Loop"]
                         /\ UNCHANGED << batch, flush_pending, flush_requester >>
                 /\ tick_fired' = tick_fired
            ELSE IF tick_fired
                 THEN /\ tick_fired' = FALSE
                      /\ IF Len(batch) > 0
                         THEN /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_FlushBatch"]
                         ELSE /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_Loop"]
                      /\ UNCHANGED << channel, batch, msg, flush_pending, flush_requester >>
                 ELSE /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_Loop"]
                      /\ UNCHANGED << channel, batch, tick_fired, msg,
                                      flush_pending, flush_requester >>
       ELSE /\ pc' = [pc EXCEPT ![NumProducers + 2] = "Done"]
            /\ UNCHANGED << channel, batch, tick_fired, msg,
                            flush_pending, flush_requester >>
    /\ UNCHANGED << in_flight, exported, dropped, total_sent,
                    terminated, shutdown_sent, producer_phase, send_ok >>

exporter_E_FlushBatch ==
    /\ pc[NumProducers + 2] = "E_FlushBatch"
    /\ in_flight < MaxConcurrent
    /\ in_flight' = in_flight + Len(batch)
    /\ batch' = <<>>
    /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_Loop"]
    /\ UNCHANGED << channel, exported, dropped, total_sent, flush_pending,
                    terminated, shutdown_sent, producer_phase, tick_fired,
                    send_ok, msg, flush_requester >>

exporter_E_HandleFlush ==
    /\ pc[NumProducers + 2] = "E_HandleFlush"
    /\ IF Len(batch) > 0
       THEN /\ in_flight < MaxConcurrent
            /\ in_flight' = in_flight + Len(batch)
            /\ batch' = <<>>
       ELSE /\ in_flight' = in_flight
            /\ batch' = batch
    /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_FlushDrain"]
    /\ UNCHANGED << channel, exported, dropped, total_sent, flush_pending,
                    terminated, shutdown_sent, producer_phase, tick_fired,
                    send_ok, msg, flush_requester >>

exporter_E_FlushDrain ==
    /\ pc[NumProducers + 2] = "E_FlushDrain"
    /\ IF Len(channel) > 0 /\ Head(channel) = MsgData
       THEN /\ batch' = Append(batch, MsgData)
            /\ channel' = Tail(channel)
            /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_FlushDrain"]
            /\ in_flight' = in_flight
       ELSE /\ IF Len(batch) > 0
               THEN /\ in_flight < MaxConcurrent
                    /\ in_flight' = in_flight + Len(batch)
                    /\ batch' = <<>>
               ELSE /\ in_flight' = in_flight
                    /\ batch' = batch
            /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_WaitInFlight"]
            /\ channel' = channel
    /\ UNCHANGED << exported, dropped, total_sent, flush_pending,
                    terminated, shutdown_sent, producer_phase, tick_fired,
                    send_ok, msg, flush_requester >>

exporter_E_WaitInFlight ==
    /\ pc[NumProducers + 2] = "E_WaitInFlight"
    /\ in_flight = 0
    /\ flush_pending' = flush_pending \ {flush_requester}
    /\ flush_requester' = 0
    /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_Loop"]
    /\ UNCHANGED << channel, batch, in_flight, exported, dropped, total_sent,
                    terminated, shutdown_sent, producer_phase, tick_fired,
                    send_ok, msg >>

exporter_E_HandleShutdown ==
    /\ pc[NumProducers + 2] = "E_HandleShutdown"
    /\ IF Len(batch) > 0
       THEN /\ in_flight < MaxConcurrent
            /\ in_flight' = in_flight + Len(batch)
            /\ batch' = <<>>
       ELSE /\ in_flight' = in_flight
            /\ batch' = batch
    /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_ShutdownDrain"]
    /\ UNCHANGED << channel, exported, dropped, total_sent, flush_pending,
                    terminated, shutdown_sent, producer_phase, tick_fired,
                    send_ok, msg, flush_requester >>

exporter_E_ShutdownDrain ==
    /\ pc[NumProducers + 2] = "E_ShutdownDrain"
    /\ IF Len(channel) > 0
       THEN /\ msg' = Head(channel)
            /\ channel' = Tail(channel)
            /\ IF msg' = MsgData
               THEN /\ batch' = Append(batch, MsgData)
               ELSE /\ batch' = batch
            /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_ShutdownDrain"]
            /\ in_flight' = in_flight
       ELSE /\ IF Len(batch) > 0
               THEN /\ in_flight < MaxConcurrent
                    /\ in_flight' = in_flight + Len(batch)
                    /\ batch' = <<>>
               ELSE /\ in_flight' = in_flight
                    /\ batch' = batch
            /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_ShutdownWait"]
            /\ UNCHANGED << channel, msg >>
    /\ UNCHANGED << exported, dropped, total_sent, flush_pending,
                    terminated, shutdown_sent, producer_phase, tick_fired,
                    send_ok, flush_requester >>

exporter_E_ShutdownWait ==
    /\ pc[NumProducers + 2] = "E_ShutdownWait"
    /\ in_flight = 0
    /\ terminated' = TRUE
    /\ pc' = [pc EXCEPT ![NumProducers + 2] = "E_Loop"]
    /\ UNCHANGED << channel, batch, in_flight, exported, dropped, total_sent,
                    flush_pending, shutdown_sent, producer_phase, tick_fired,
                    send_ok, msg, flush_requester >>

exporter_loop_proc ==
    \/ exporter_E_Loop
    \/ exporter_E_FlushBatch
    \/ exporter_E_HandleFlush
    \/ exporter_E_FlushDrain
    \/ exporter_E_WaitInFlight
    \/ exporter_E_HandleShutdown
    \/ exporter_E_ShutdownDrain
    \/ exporter_E_ShutdownWait

\* Complete Next-state relation
Next ==
    \/ (\E self \in Producers: producer(self))
    \/ timer
    \/ completer
    \/ exporter_loop_proc

Spec == /\ Init
        /\ [][Next]_vars
        /\ \A self \in Producers : WF_vars(producer(self))
        /\ WF_vars(timer_T_Loop)
        /\ WF_vars(timer_T_Wait)
        /\ WF_vars(completer)
        /\ WF_vars(exporter_E_Loop)
        /\ WF_vars(exporter_E_FlushBatch)
        /\ WF_vars(exporter_E_HandleFlush)
        /\ WF_vars(exporter_E_FlushDrain)
        /\ WF_vars(exporter_E_WaitInFlight)
        /\ WF_vars(exporter_E_HandleShutdown)
        /\ WF_vars(exporter_E_ShutdownDrain)
        /\ WF_vars(exporter_E_ShutdownWait)

\* END TRANSLATION

\* ===============================================================
\* Safety Properties
\* ===============================================================

\* 1. Channel never exceeds its bounded capacity
ChannelBounded == Len(channel) <= Capacity

\* 2. Conservation: no messages lost or created out of thin air
\*    dropped + in_channel + in_batch + in_flight + exported = total_sent
Conservation ==
    dropped + Len(channel) + Len(batch) + in_flight + exported
    >= total_sent - Cardinality(flush_pending)

\* Tight conservation for Data messages only (Flush/Shutdown are control msgs)
\* Count only data messages in the channel
DataInChannel == Len(SelectSeq(channel, LAMBDA m: m = MsgData))

DataConservation ==
    dropped + DataInChannel + Len(batch) + in_flight + exported = total_sent

\* 3. In-flight exports never exceed semaphore bound
InFlightBounded == in_flight <= MaxConcurrent + BatchSize

\* ===============================================================
\* Liveness Properties (require fairness)
\* ===============================================================

\* 4. Every Flush eventually gets a reply
FlushCompletes ==
    \A p \in Producers :
        (p \in flush_pending) ~> (p \notin flush_pending)

\* 5. After Shutdown is sent, exporter eventually terminates
ShutdownTerminates ==
    shutdown_sent ~> terminated

\* 6. Deadlock freedom: the system can always make progress or has terminated
AllTerminated ==
    \A self \in ProcSet : pc[self] = "Done"

DeadlockFreedom == [](~AllTerminated => ENABLED(Next))

=======================================================================
