---------------------------- MODULE CaptureCancel ----------------------------
EXTENDS Naturals, Sequences, TLC

CONSTANTS MaxControls, Scenario, DropCompletionGuard, ForgetEarlierStop
Commands == {"cancel", "pause", "resume", "toggle", "apply", "quit"}
VARIABLES phase, queue, sent, off, quit, preEnds, postEnds, intentOff,
          stopRequested, cancelPending, cueDone, cueAccepted, closeDone,
          secondary, result, delivered
vars == <<phase, queue, sent, off, quit, preEnds, postEnds, intentOff,
          stopRequested, cancelPending, cueDone, cueAccepted, closeDone,
          secondary, result, delivered>>

NextOff(c, old) == CASE c = "pause" -> TRUE
                       [] c = "resume" -> FALSE
                       [] c = "toggle" -> ~old
                       [] OTHER -> old
Ends(c, old) == c \in {"cancel", "pause", "quit"}
                \/ (c = "toggle" /\ ~old)
Accumulate(old, ending) == IF ForgetEarlierStop THEN ending ELSE old \/ ending

Init == /\ phase = "pre"
        /\ queue = <<>> /\ sent = 0 /\ off = FALSE /\ quit = FALSE
        /\ preEnds = FALSE /\ postEnds = FALSE /\ intentOff = FALSE
        /\ stopRequested = FALSE /\ cancelPending = FALSE
        /\ cueDone = FALSE /\ cueAccepted = FALSE /\ closeDone = FALSE
        /\ secondary \in BOOLEAN /\ result = "none" /\ delivered = FALSE

\* A producer can enqueue between any two receives, including drain iterations.
Send(c) == /\ phase # "done" /\ sent < MaxControls
           /\ queue' = Append(queue, c) /\ sent' = sent + 1
           /\ intentOff' = NextOff(c, intentOff)
           /\ stopRequested' = (stopRequested \/ Ends(c, intentOff))
           /\ UNCHANGED <<phase, off, quit, preEnds, postEnds, cancelPending,
                          cueDone, cueAccepted, closeDone, secondary, result, delivered>>

\* One try_recv + Requests::take; the producer does not access Requests.
DrainOne == /\ phase \in {"pre", "finish"} /\ Len(queue) > 0
            /\ LET c == Head(queue) IN
               /\ off' = NextOff(c, off) /\ quit' = (quit \/ (c = "quit"))
               /\ preEnds' = IF phase = "pre"
                              THEN Accumulate(preEnds, Ends(c, off)) ELSE preEnds
               /\ postEnds' = IF phase = "finish"
                               THEN Accumulate(postEnds, Ends(c, off)) ELSE postEnds
            /\ queue' = Tail(queue)
            /\ UNCHANGED <<phase, sent, intentOff, stopRequested, cancelPending,
                           cueDone, cueAccepted, closeDone, secondary, result, delivered>>

\* No control branch precedes the capture's first poll (biased select).
PreEmpty == /\ phase = "pre" /\ queue = <<>>
            /\ phase' = IF preEnds THEN "done" ELSE "active"
            /\ UNCHANGED <<queue, sent, off, quit, preEnds, postEnds, intentOff,
                           stopRequested, cancelPending, cueDone, cueAccepted,
                           closeDone, secondary, result, delivered>>

\* The engine is Pending here: in capture, ending join, or secondary STT.
Receive == /\ phase \in {"active", "ending", "secondary"}
           /\ ~quit /\ Len(queue) > 0
           /\ (~cancelPending \/ (phase = "ending" /\ cueDone))
           /\ (Scenario = "all" \/ ~cueDone)
           /\ LET c == Head(queue) IN
              /\ off' = NextOff(c, off) /\ quit' = (quit \/ (c = "quit"))
              /\ cancelPending' = (cancelPending \/ Ends(c, off))
           /\ queue' = Tail(queue)
           /\ UNCHANGED <<phase, sent, preEnds, postEnds, intentOff, stopRequested,
                          cueDone, cueAccepted, closeDone, secondary, result, delivered>>

\* Abstract the pre-accept capture awaits: observed cancellation cannot accept.
Accept == /\ phase = "active" /\ ~cancelPending /\ phase' = "ending"
          /\ UNCHANGED <<queue, sent, off, quit, preEnds, postEnds, intentOff,
                         stopRequested, cancelPending, cueDone, cueAccepted,
                         closeDone, secondary, result, delivered>>
ActiveCancelled == /\ phase = "active" /\ cancelPending
                   /\ phase' = "finish" /\ result' = "cancelled"
                   /\ UNCHANGED <<queue, sent, off, quit, preEnds, postEnds, intentOff,
                                  stopRequested, cancelPending, cueDone, cueAccepted,
                                  closeDone, secondary, delivered>>

\* Engine::end_cue polls cancellation first, but no longer polls once completed.
CueReturns == /\ phase = "ending" /\ ~cueDone
              /\ cueDone' = TRUE /\ cueAccepted' = ~cancelPending
              /\ phase' = IF ~closeDone THEN "ending"
                           ELSE IF ~cancelPending /\ secondary THEN "secondary" ELSE "finish"
              /\ result' = IF ~closeDone \/ (~cancelPending /\ secondary) THEN "none"
                            ELSE IF cancelPending THEN "cancelled" ELSE "accepted"
              /\ UNCHANGED <<queue, sent, off, quit, preEnds, postEnds, intentOff,
                             stopRequested, cancelPending, closeDone, secondary, delivered>>

\* end_with's join retains the cue's earlier bool while close is Pending.
CloseReturns == /\ phase = "ending" /\ ~closeDone
                /\ closeDone' = TRUE
                /\ phase' = IF ~cueDone THEN "ending"
                             ELSE IF cueAccepted /\ secondary THEN "secondary" ELSE "finish"
                /\ result' = IF ~cueDone \/ (cueAccepted /\ secondary) THEN "none"
                              ELSE IF cueAccepted THEN "accepted" ELSE "cancelled"
                /\ UNCHANGED <<queue, sent, off, quit, preEnds, postEnds, intentOff,
                               stopRequested, cancelPending, cueDone, cueAccepted,
                               secondary, delivered>>

SecondaryReturns == /\ phase = "secondary" /\ phase' = "finish"
                    /\ result' = IF cancelPending THEN "cancelled" ELSE "accepted"
                    /\ UNCHANGED <<queue, sent, off, quit, preEnds, postEnds, intentOff,
                                   stopRequested, cancelPending, cueDone, cueAccepted,
                                   closeDone, secondary, delivered>>

\* Last empty try_recv is the admission fence; later sends belong to the next flow.
FinishEmpty == /\ phase = "finish" /\ queue = <<>> /\ phase' = "done"
               /\ delivered' = (result = "accepted" /\ ~quit
                                /\ (DropCompletionGuard \/ ~postEnds))
               /\ UNCHANGED <<queue, sent, off, quit, preEnds, postEnds, intentOff,
                              stopRequested, cancelPending, cueDone, cueAccepted,
                              closeDone, secondary, result>>

Next == (\E c \in Commands : Send(c)) \/ DrainOne \/ PreEmpty \/ Receive
        \/ Accept \/ ActiveCancelled \/ CueReturns \/ CloseReturns
        \/ SecondaryReturns \/ FinishEmpty
Spec == Init /\ [][Next]_vars
NoDeliveryAfterStop == delivered => ~stopRequested
=============================================================================
