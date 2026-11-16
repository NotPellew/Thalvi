---------------------------- MODULE CaptureCancel ----------------------------
EXTENDS Naturals, Sequences, TLC

CONSTANTS MaxControls, Scenario, DropCompletionGuard, ForgetEarlierStop, LeakExtraOnCancel
Commands == {"cancel", "pause", "resume", "toggle", "apply", "restart", "quit"}
VARIABLES phase, queue, sent, off, quit, preEnds, ended, intentOff,
          stopRequested, cancelPending, cueDone, cueAccepted, closeDone,
          secondary, writing, result, delivered, extras, extra, merged, starting
vars == <<phase, queue, sent, off, quit, preEnds, ended, intentOff,
          stopRequested, cancelPending, cueDone, cueAccepted, closeDone,
          secondary, writing, result, delivered, extras, extra, merged, starting>>
extraVars == <<extras, extra, merged, starting>>

NextOff(c, old) == CASE c = "pause" -> TRUE
                       [] c = "resume" -> FALSE
                       [] c = "toggle" -> ~old
                       [] OTHER -> old
Ends(c, old) == c \in {"cancel", "pause", "quit"}
                \/ (c = "toggle" /\ ~old)
Accumulate(old, ending) == IF ForgetEarlierStop THEN ending ELSE old \/ ending
\* Streams::cancel issues Notify and transfers task ownership; it does not await session cleanup.
Released(x) == IF x = "open" /\ ~LeakExtraOnCancel THEN "released" ELSE x

Init == /\ phase = "pre"
        /\ queue = <<>> /\ sent = 0 /\ off = FALSE /\ quit = FALSE
        /\ preEnds = FALSE /\ ended = FALSE /\ intentOff = FALSE
        /\ stopRequested = FALSE /\ cancelPending = FALSE
        /\ cueDone = FALSE /\ cueAccepted = FALSE /\ closeDone = FALSE
        /\ secondary \in BOOLEAN /\ writing \in BOOLEAN /\ result = "none" /\ delivered = FALSE
        \* `extras`: streaming extras are on. Without `secondary` they exist but the merge does not run
        \* (a scratched capture, or one with no words of its own).
        /\ extras \in BOOLEAN /\ extra = "none" /\ merged = FALSE
        \* `starting`: the recognizer loads a model on this computer, so Engine::start waits for the
        \* load with no time limit (a remote recognizer is abandoned after READY_TIMEOUT: a failure).
        /\ starting \in BOOLEAN

\* A producer can enqueue between any two receives, including drain iterations.
Send(c) == /\ phase # "done" /\ sent < MaxControls
           /\ queue' = Append(queue, c) /\ sent' = sent + 1
           /\ intentOff' = NextOff(c, intentOff)
           /\ stopRequested' = (stopRequested \/ Ends(c, intentOff))
           /\ UNCHANGED <<phase, off, quit, preEnds, ended, cancelPending,
                          cueDone, cueAccepted, closeDone, secondary, writing, result, delivered,
                          extraVars>>

\* One try_recv + Requests::take; the producer does not access Requests. Requests::take keeps
\* every ending decision in `ended`, which lasts through the whole drive call.
DrainOne == /\ phase \in {"pre", "finish"} /\ Len(queue) > 0
            /\ LET c == Head(queue) IN
               /\ off' = NextOff(c, off) /\ quit' = (quit \/ (c = "quit"))
               /\ preEnds' = IF phase = "pre"
                              THEN preEnds \/ Ends(c, off) ELSE preEnds
               /\ ended' = Accumulate(ended, Ends(c, off))
            /\ queue' = Tail(queue)
            /\ UNCHANGED <<phase, sent, intentOff, stopRequested, cancelPending,
                           cueDone, cueAccepted, closeDone, secondary, writing, result, delivered,
                           extraVars>>

\* No control branch precedes the capture's first poll (biased select). Engine::capture starts
\* the streaming extras' tasks when it is first polled, so a capture that never starts has none.
PreEmpty == /\ phase = "pre" /\ queue = <<>>
            /\ phase' = IF preEnds THEN "done" ELSE "active"
            /\ extra' = IF ~preEnds /\ extras THEN "open" ELSE extra
            /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                           stopRequested, cancelPending, cueDone, cueAccepted,
                           closeDone, secondary, writing, result, delivered, extras, merged, starting>>

\* The engine is Pending here: capture, ending join, second pass or writing.
Receive == /\ phase \in {"active", "ending", "secondary", "writing"}
           /\ ~quit /\ Len(queue) > 0
           /\ (~cancelPending \/ (phase = "ending" /\ cueDone))
           /\ (Scenario = "all" \/ ~cueDone)
           /\ LET c == Head(queue) IN
              /\ off' = NextOff(c, off) /\ quit' = (quit \/ (c = "quit"))
              /\ cancelPending' = (cancelPending \/ Ends(c, off))
              /\ ended' = Accumulate(ended, Ends(c, off))
           /\ queue' = Tail(queue)
           /\ UNCHANGED <<phase, sent, preEnds, intentOff, stopRequested,
                          cueDone, cueAccepted, closeDone, secondary, writing, result, delivered,
                          extraVars>>

\* Engine::start with a model still loading: the select polls cancellation first and has no time
\* limit, so this wait may last any number of steps. It ends when the model is ready, when the load
\* fails or reaches a download (a failed capture that delivers nothing, ending in conclude, which
\* cancels the extras), or by a cancel observed first (ActiveCancelled).
\* Frames read during the wait, including after a hold is released, are not a step here: the cutoff
\* discards what follows the release, and that read neither admits delivery nor changes this wait.
Loaded == /\ phase = "active" /\ starting /\ ~cancelPending /\ starting' = FALSE
          /\ UNCHANGED <<phase, queue, sent, off, quit, preEnds, ended, intentOff,
                         stopRequested, cancelPending, cueDone, cueAccepted,
                         closeDone, secondary, writing, result, delivered, extras, extra, merged>>
LoadFailed == /\ phase = "active" /\ starting /\ ~cancelPending
              /\ phase' = "finish" /\ result' = "failed" /\ starting' = FALSE
              /\ extra' = Released(extra)
              /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                             stopRequested, cancelPending, cueDone, cueAccepted,
                             closeDone, secondary, writing, delivered, extras, merged>>

\* Abstract the pre-accept capture awaits: observed cancellation cannot accept. Engine::conclude
\* sends Finish to the extras when the merge will run, and cancels them when it will not.
Accept == /\ phase = "active" /\ ~starting /\ ~cancelPending /\ phase' = "ending"
          /\ extra' = IF extras /\ ~secondary THEN Released(extra) ELSE extra
          /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                         stopRequested, cancelPending, cueDone, cueAccepted,
                         closeDone, secondary, writing, result, delivered, extras, merged, starting>>
\* Every end without an accept (cancel, failure, "never mind") cancels the extras in conclude.
ActiveCancelled == /\ phase = "active" /\ cancelPending
                   /\ phase' = "finish" /\ result' = "cancelled"
                   /\ extra' = Released(extra)
                   /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                  stopRequested, cancelPending, cueDone, cueAccepted,
                                  closeDone, secondary, writing, delivered, extras, merged, starting>>

AfterEnding == IF secondary THEN "secondary" ELSE IF writing THEN "writing" ELSE "finish"

\* Engine::end_cue polls cancellation first, but no longer polls once completed. When it ends the
\* capture as cancelled, conclude cancels the extras.
CueReturns == /\ phase = "ending" /\ ~cueDone
              /\ cueDone' = TRUE /\ cueAccepted' = ~cancelPending
              /\ phase' = IF ~closeDone THEN "ending"
                           ELSE IF ~cancelPending /\ (secondary \/ writing) THEN AfterEnding ELSE "finish"
              /\ result' = IF ~closeDone \/ (~cancelPending /\ (secondary \/ writing)) THEN "none"
                            ELSE IF cancelPending THEN "cancelled" ELSE "accepted"
              /\ extra' = IF phase' = "finish" /\ result' = "cancelled" THEN Released(extra) ELSE extra
              /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                             stopRequested, cancelPending, closeDone, secondary, writing, delivered,
                             extras, merged, starting>>

\* end_with's join retains the cue's earlier bool while close is Pending.
CloseReturns == /\ phase = "ending" /\ ~closeDone
                /\ closeDone' = TRUE
                /\ phase' = IF ~cueDone THEN "ending"
                             ELSE IF cueAccepted /\ (secondary \/ writing) THEN AfterEnding ELSE "finish"
                /\ result' = IF ~cueDone \/ (cueAccepted /\ (secondary \/ writing)) THEN "none"
                              ELSE IF cueAccepted THEN "accepted" ELSE "cancelled"
                /\ extra' = IF phase' = "finish" /\ result' = "cancelled" THEN Released(extra) ELSE extra
                /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                               stopRequested, cancelPending, cueDone, cueAccepted,
                               secondary, writing, delivered, extras, merged, starting>>

\* Second-pass completion and writing completion are distinct cancellation-first awaits. A cancel
\* here cancels the extras still running (Streams::cancel); a merge that ran leaves the ones it did
\* not wait for to finish on their own; they feed no capture outcome. Cleanup is not modeled.
SecondaryReturns == /\ phase = "secondary"
                    /\ phase' = IF ~cancelPending /\ writing THEN "writing" ELSE "finish"
                    /\ result' = IF cancelPending THEN "cancelled"
                                  ELSE IF writing THEN "none" ELSE "accepted"
                    /\ extra' = IF cancelPending THEN Released(extra)
                                 ELSE IF extra = "open" THEN "late" ELSE extra
                    /\ merged' = (~cancelPending /\ extras)
                    /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                   stopRequested, cancelPending, cueDone, cueAccepted,
                                   closeDone, secondary, writing, delivered, extras, starting>>

WritingReturns == /\ phase = "writing" /\ phase' = "finish"
                  /\ result' = IF cancelPending THEN "cancelled" ELSE "accepted"
                  /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                 stopRequested, cancelPending, cueDone, cueAccepted,
                                 closeDone, secondary, writing, delivered, extraVars>>

\* Last empty try_recv is the admission fence; later sends belong to the next flow. Settling
\* delivers an outcome only if no ending control was consumed anywhere in drive.
FinishEmpty == /\ phase = "finish" /\ queue = <<>> /\ phase' = "done"
               /\ delivered' = (result = "accepted"
                                /\ (DropCompletionGuard \/ ~ended))
               /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                              stopRequested, cancelPending, cueDone, cueAccepted,
                              closeDone, secondary, writing, result, extraVars>>

Next == (\E c \in Commands : Send(c)) \/ DrainOne \/ PreEmpty \/ Receive
        \/ Loaded \/ LoadFailed \/ Accept \/ ActiveCancelled \/ CueReturns \/ CloseReturns
        \/ SecondaryReturns \/ WritingReturns \/ FinishEmpty
Spec == Init /\ [][Next]_vars
NoDeliveryAfterStop == delivered => ~stopRequested
\* No extra remains owned by the finished capture: cancellation was requested or ownership handed on.
ExtrasNeverLeftOpen == phase = "done" => extra # "open"
\* A capture cancelled before merge handoff has requested cancellation of each extra.
ReleasedWhenCancelled == (phase = "done" /\ result = "cancelled" /\ ~merged)
                         => extra \in {"none", "released"}
\* Nothing is accepted while the model is still loading.
AcceptedOnlyAfterLoad == phase \in {"ending", "secondary", "writing"} => ~starting
\* Only a completed merge stage (including skipped text merging) hands extras off without cancellation.
LateOnlyAfterMerge == extra = "late" => merged
=============================================================================
