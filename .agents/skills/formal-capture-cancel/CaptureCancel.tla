---------------------------- MODULE CaptureCancel ----------------------------
EXTENDS Naturals, Sequences, TLC

CONSTANTS MaxControls, Scenario, DropCompletionGuard, ForgetEarlierStop, LeakExtraOnCancel
Commands == {"cancel", "pause", "resume", "toggle", "apply", "restart", "quit"}
VARIABLES phase, queue, sent, off, quit, preEnds, ended, intentOff,
          stopRequested, cancelPending, cueDone, cueAccepted, closeDone,
          secondary, writing, cleaning, result, delivered, extras, extra, merged, starting
vars == <<phase, queue, sent, off, quit, preEnds, ended, intentOff,
          stopRequested, cancelPending, cueDone, cueAccepted, closeDone,
          secondary, writing, cleaning, result, delivered, extras, extra, merged, starting>>
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
        \* `cleaning` is opt-in dictation cleanup of a submit, before its confirmation is shown.
        \* A paste is cleaned after the ending instead, which this one stage also stands for.
        \* It runs only when an explicit write does not.
        /\ secondary \in BOOLEAN /\ writing \in BOOLEAN /\ cleaning \in BOOLEAN /\ (~writing \/ ~cleaning)
        /\ result = "none" /\ delivered = FALSE
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
                          cueDone, cueAccepted, closeDone, secondary, writing, cleaning, result, delivered,
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
                           cueDone, cueAccepted, closeDone, secondary, writing, cleaning, result, delivered,
                           extraVars>>

\* No control branch precedes the capture's first poll (biased select). Engine::capture starts
\* the streaming extras' tasks when it is first polled, so a capture that never starts has none.
PreEmpty == /\ phase = "pre" /\ queue = <<>>
            /\ phase' = IF preEnds THEN "done" ELSE "active"
            /\ extra' = IF ~preEnds /\ extras THEN "open" ELSE extra
            /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                           stopRequested, cancelPending, cueDone, cueAccepted,
                           closeDone, secondary, writing, cleaning, result, delivered, extras, merged, starting>>

\* The engine is Pending here: capture, a submit's cleanup, recognizer release, second pass or writing.
Receive == /\ phase \in {"active", "cleaning", "ending", "secondary", "writing"}
           /\ ~quit /\ Len(queue) > 0
           /\ (~cancelPending \/ phase = "ending")
           /\ (Scenario = "all" \/ ~cueDone)
           /\ LET c == Head(queue) IN
              /\ off' = NextOff(c, off) /\ quit' = (quit \/ (c = "quit"))
              /\ cancelPending' = (cancelPending \/ Ends(c, off))
              /\ ended' = Accumulate(ended, Ends(c, off))
           /\ queue' = Tail(queue)
           /\ UNCHANGED <<phase, sent, preEnds, intentOff, stopRequested,
                          cueDone, cueAccepted, closeDone, secondary, writing, cleaning, result, delivered,
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
                         closeDone, secondary, writing, cleaning, result, delivered, extras, extra, merged>>
LoadFailed == /\ phase = "active" /\ starting /\ ~cancelPending
              /\ phase' = "finish" /\ result' = "failed" /\ starting' = FALSE
              /\ extra' = Released(extra)
              /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                             stopRequested, cancelPending, cueDone, cueAccepted,
                             closeDone, secondary, writing, cleaning, delivered, extras, merged>>

\* A submit with cleanup on is cleaned before its confirmation is shown. The confirmation is the
\* ending pause, so this await is before Accept. A cancel here yields a cancelled result and
\* delivers nothing. A paste's cleanup is after the ending; this stage stands for the submit.
Prepare == /\ phase = "active" /\ ~starting /\ ~cancelPending /\ cleaning /\ ~secondary
           /\ phase' = "cleaning"
           /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                          stopRequested, cancelPending, cueDone, cueAccepted,
                          closeDone, secondary, writing, cleaning, result, delivered, extraVars>>

\* Dictation cleanup. Engine::clean polls cancellation first; a cancel pastes nothing.
CleanupReturns == /\ phase = "cleaning"
                  /\ phase' = IF cancelPending THEN "finish" ELSE "active"
                  /\ result' = IF cancelPending THEN "cancelled" ELSE "none"
                  /\ cleaning' = FALSE
                  /\ extra' = IF cancelPending THEN Released(extra) ELSE extra
                  /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                 stopRequested, cancelPending, cueDone, cueAccepted,
                                 closeDone, secondary, writing, delivered, extras, merged, starting>>

\* Abstract the pre-accept capture awaits: observed cancellation cannot accept. Engine::conclude
\* sends Finish to the extras when the merge will run, and cancels them when it will not.
Accept == /\ phase = "active" /\ ~starting /\ ~cancelPending /\ (~cleaning \/ secondary) /\ phase' = "ending"
          /\ extra' = IF extras /\ ~secondary THEN Released(extra) ELSE extra
          /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                         stopRequested, cancelPending, cueDone, cueAccepted,
                         closeDone, secondary, writing, cleaning, result, delivered, extras, merged, starting>>
\* Every end without an accept (cancel, failure, "never mind") cancels the extras in conclude.
ActiveCancelled == /\ phase = "active" /\ cancelPending
                   /\ phase' = "finish" /\ result' = "cancelled"
                   /\ extra' = Released(extra)
                   /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                  stopRequested, cancelPending, cueDone, cueAccepted,
                                  closeDone, secondary, writing, cleaning, delivered, extras, merged, starting>>

AfterEnding == IF secondary THEN "secondary" ELSE IF writing THEN "writing" ELSE "finish"

\* The accepted cue runs independently: its completion or cancellation only records feedback.
\* It may end while close, later processing or delivery admission has already finished.
CueReturns == /\ (phase = "ending" \/ closeDone) /\ ~cueDone
              /\ cueDone' = TRUE /\ cueAccepted' = ~cancelPending
              /\ UNCHANGED <<phase, queue, sent, off, quit, preEnds, ended, intentOff,
                             stopRequested, cancelPending, closeDone, secondary, writing, cleaning,
                             result, delivered, extras, extra, merged, starting>>

\* Accepted end_with awaits only recognizer release, polling cancellation first.
\* The runner's completion drain still guards admission when release returns Accepted.
CloseReturns == /\ phase = "ending" /\ ~closeDone
                /\ closeDone' = TRUE
                /\ phase' = IF ~cancelPending /\ (secondary \/ writing) THEN AfterEnding ELSE "finish"
                /\ result' = IF ~cancelPending /\ (secondary \/ writing) THEN "none"
                              ELSE IF cancelPending THEN "cancelled" ELSE "accepted"
                /\ extra' = IF cancelPending THEN Released(extra) ELSE extra
                /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                               stopRequested, cancelPending, cueDone, cueAccepted,
                               secondary, writing, cleaning, delivered, extras, merged, starting>>

\* Second-pass completion and writing completion are distinct cancellation-first awaits. A cancel
\* here cancels the extras still running (Streams::cancel); a merge that ran leaves the ones it did
\* not wait for to finish on their own; they feed no capture outcome. Cleanup is not modeled.
SecondaryReturns == /\ phase = "secondary"
                    /\ phase' = IF cancelPending THEN "finish"
                                 ELSE IF writing THEN "writing" ELSE "finish"
                    /\ result' = IF cancelPending THEN "cancelled"
                                  ELSE IF writing THEN "none" ELSE "accepted"
                    /\ extra' = IF cancelPending THEN Released(extra)
                                 ELSE IF extra = "open" THEN "late" ELSE extra
                    /\ merged' = (~cancelPending /\ extras)
                    /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                   stopRequested, cancelPending, cueDone, cueAccepted,
                                   closeDone, secondary, writing, cleaning, delivered, extras, starting>>

\* An explicit write. It never continues into dictation cleanup: cleanup is the other path.
WritingReturns == /\ phase = "writing" /\ phase' = "finish"
                  /\ result' = IF cancelPending THEN "cancelled" ELSE "accepted"
                  /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                 stopRequested, cancelPending, cueDone, cueAccepted,
                                 closeDone, secondary, writing, cleaning, delivered, extraVars>>

\* Last empty try_recv is the admission fence; later sends belong to the next flow. Settling
\* delivers an outcome only if no ending control was consumed anywhere in drive.
FinishEmpty == /\ phase = "finish" /\ queue = <<>> /\ phase' = "done"
               /\ delivered' = (result = "accepted"
                                /\ (DropCompletionGuard \/ ~ended))
               /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                              stopRequested, cancelPending, cueDone, cueAccepted,
                              closeDone, secondary, writing, cleaning, result, extraVars>>

Next == (\E c \in Commands : Send(c)) \/ DrainOne \/ PreEmpty \/ Receive
        \/ Loaded \/ LoadFailed \/ Prepare \/ CleanupReturns \/ Accept \/ ActiveCancelled
        \/ CueReturns \/ CloseReturns
        \/ SecondaryReturns \/ WritingReturns \/ FinishEmpty
Spec == Init /\ [][Next]_vars
NoDeliveryAfterStop == delivered => ~stopRequested
\* No extra remains owned by the finished capture: cancellation was requested or ownership handed on.
ExtrasNeverLeftOpen == phase = "done" => extra # "open"
\* A capture cancelled before merge handoff has requested cancellation of each extra.
ReleasedWhenCancelled == (phase = "done" /\ result = "cancelled" /\ ~merged)
                         => extra \in {"none", "released"}
\* Nothing is accepted while the model is still loading.
AcceptedOnlyAfterLoad == phase \in {"cleaning", "ending", "secondary", "writing"} => ~starting
\* Only a completed merge stage (including skipped text merging) hands extras off without cancellation.
LateOnlyAfterMerge == extra = "late" => merged
=============================================================================
