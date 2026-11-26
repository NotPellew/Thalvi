---------------------------- MODULE CaptureCancel ----------------------------
EXTENDS Naturals, Sequences, TLC

CONSTANTS MaxControls, Scenario, DropCompletionGuard, ForgetEarlierStop
Commands == {"cancel", "pause", "resume", "toggle", "apply", "restart", "quit"}
VARIABLES phase, queue, sent, off, quit, preEnds, ended, intentOff,
          stopRequested, cancelPending, cueDone, cueAccepted, closeDone,
          writing, cleaning, submit, result, delivered, starting
vars == <<phase, queue, sent, off, quit, preEnds, ended, intentOff,
          stopRequested, cancelPending, cueDone, cueAccepted, closeDone,
          writing, cleaning, submit, result, delivered, starting>>

NextOff(c, old) == CASE c = "pause" -> TRUE
                       [] c = "resume" -> FALSE
                       [] c = "toggle" -> ~old
                       [] OTHER -> old
Ends(c, old) == c \in {"cancel", "pause", "quit"}
                \/ (c = "toggle" /\ ~old)
Accumulate(old, ending) == IF ForgetEarlierStop THEN ending ELSE old \/ ending

Init == /\ phase = "pre"
        /\ queue = <<>> /\ sent = 0 /\ off = FALSE /\ quit = FALSE
        /\ preEnds = FALSE /\ ended = FALSE /\ intentOff = FALSE
        /\ stopRequested = FALSE /\ cancelPending = FALSE
        /\ cueDone = FALSE /\ cueAccepted = FALSE /\ closeDone = FALSE
        \* `cleaning` is opt-in dictation cleanup of a submit before its confirmation; a paste is
        \* cleaned after the ending instead, which this one stage stands for.
        \* It runs only when an explicit write does not.
        /\ writing \in BOOLEAN /\ cleaning \in BOOLEAN /\ (~writing \/ ~cleaning)
        \* `submit`: the capture ends with Enter, so its full text is shown and confirmed.
        /\ submit \in BOOLEAN
        /\ result = "none" /\ delivered = FALSE
        \* `starting`: the recognizer loads a model on this computer, so Engine::start waits for the
        \* load with no time limit (a remote recognizer is abandoned after READY_TIMEOUT: a failure).
        /\ starting \in BOOLEAN

\* A producer can enqueue between any two receives, including drain iterations.
\* Theme-only Apply retunes after capture. Apply is outside the ending controls in Send.
Send(c) == /\ phase # "done" /\ sent < MaxControls
           /\ queue' = Append(queue, c) /\ sent' = sent + 1
           /\ intentOff' = NextOff(c, intentOff)
           /\ stopRequested' = (stopRequested \/ Ends(c, intentOff))
           /\ UNCHANGED <<phase, off, quit, preEnds, ended, cancelPending,
                          cueDone, cueAccepted, closeDone, writing, cleaning, submit, result, delivered, starting>>

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
                           cueDone, cueAccepted, closeDone, writing, cleaning, submit, result, delivered, starting>>

\* Engine::listen cuts and drains the previous capture's cue before this flow starts.
\* Its watch version is consumed at capture entry; this model covers only the new capture.
\* No control branch precedes the capture's first poll (biased select).
PreEmpty == /\ phase = "pre" /\ queue = <<>>
            /\ phase' = IF preEnds THEN "done" ELSE "active"
            /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                           stopRequested, cancelPending, cueDone, cueAccepted,
                           closeDone, writing, cleaning, submit, result, delivered, starting>>

\* The engine is Pending here: capture, a submit's cleanup, recognizer release or writing.
Receive == /\ phase \in {"active", "cleaning", "ending", "preview", "confirm", "writing"}
           /\ ~quit /\ Len(queue) > 0
           /\ (~cancelPending \/ phase = "ending")
           /\ (Scenario = "all" \/ ~cueDone)
           /\ LET c == Head(queue) IN
              /\ off' = NextOff(c, off) /\ quit' = (quit \/ (c = "quit"))
              /\ cancelPending' = (cancelPending \/ Ends(c, off))
              /\ ended' = Accumulate(ended, Ends(c, off))
           /\ queue' = Tail(queue)
           /\ UNCHANGED <<phase, sent, preEnds, intentOff, stopRequested,
                          cueDone, cueAccepted, closeDone, writing, cleaning, submit, result, delivered, starting>>

\* Engine::start with a model still loading: the select polls cancellation first and has no time
\* limit, so this wait may last any number of steps. It ends when the model is ready, when the load
\* fails or reaches a download (a failed capture that delivers nothing), or by a cancel observed
\* first (ActiveCancelled).
\* Frames read during the wait, including after a hold is released, are not a step here: the cutoff
\* discards what follows the release, and that read neither admits delivery nor changes this wait.
Loaded == /\ phase = "active" /\ starting /\ ~cancelPending /\ starting' = FALSE
          /\ UNCHANGED <<phase, queue, sent, off, quit, preEnds, ended, intentOff,
                         stopRequested, cancelPending, cueDone, cueAccepted,
                         closeDone, writing, cleaning, submit, result, delivered>>
LoadFailed == /\ phase = "active" /\ starting /\ ~cancelPending
              /\ phase' = "finish" /\ result' = "failed" /\ starting' = FALSE
              /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                             stopRequested, cancelPending, cueDone, cueAccepted,
                             closeDone, writing, cleaning, submit, delivered>>

\* A submit with cleanup on is cleaned before its confirmation is shown. The confirmation is the
\* ending pause, so this await is before Accept. A cancel here yields a cancelled result and
\* delivers nothing.
Prepare == /\ phase = "active" /\ ~starting /\ ~cancelPending /\ cleaning /\ submit
           /\ phase' = "cleaning"
           /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                          stopRequested, cancelPending, cueDone, cueAccepted,
                          closeDone, writing, cleaning, submit, result, delivered, starting>>

\* Dictation cleanup. Engine::clean polls cancellation first; a cancel pastes nothing.
CleanupReturns == /\ phase = "cleaning"
                  /\ phase' = IF cancelPending THEN "finish" ELSE "active"
                  /\ result' = IF cancelPending THEN "cancelled" ELSE "none"
                  /\ cleaning' = FALSE
                  /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                 stopRequested, cancelPending, cueDone, cueAccepted,
                                 closeDone, writing, submit, delivered, starting>>

\* Abstract the pre-accept capture awaits: observed cancellation cannot accept. A submit's
\* cleanup has already returned before this accept.
Accept == /\ phase = "active" /\ ~starting /\ ~cancelPending /\ ~cleaning /\ phase' = "ending"
          /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                         stopRequested, cancelPending, cueDone, cueAccepted,
                         closeDone, writing, cleaning, submit, result, delivered, starting>>
\* Every end without an accept (cancel, failure, "never mind") delivers nothing.
ActiveCancelled == /\ phase = "active" /\ cancelPending
                   /\ phase' = "finish" /\ result' = "cancelled"
                   /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                  stopRequested, cancelPending, cueDone, cueAccepted,
                                  closeDone, writing, cleaning, submit, delivered, starting>>

AfterEnding == IF writing THEN "writing"
               ELSE IF submit THEN "preview"
               ELSE "finish"

\* The accepted cue runs independently: its completion or cancellation only records feedback.
\* It may end while close, later processing or delivery admission has already finished.
CueReturns == /\ (phase = "ending" \/ closeDone) /\ ~cueDone
              /\ cueDone' = TRUE /\ cueAccepted' = ~cancelPending
              /\ UNCHANGED <<phase, queue, sent, off, quit, preEnds, ended, intentOff,
                             stopRequested, cancelPending, closeDone, writing, cleaning,
                             submit, result, delivered, starting>>

\* Accepted end_with awaits only recognizer release, polling cancellation first.
\* The runner's completion drain still guards admission when release returns Accepted.
CloseReturns == /\ phase = "ending" /\ ~closeDone
                /\ closeDone' = TRUE
                /\ phase' = IF ~cancelPending /\ (writing \/ submit) THEN AfterEnding ELSE "finish"
                /\ result' = IF ~cancelPending /\ (writing \/ submit) THEN "none"
                              ELSE IF cancelPending THEN "cancelled" ELSE "accepted"
                /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                               stopRequested, cancelPending, cueDone, cueAccepted,
                               writing, cleaning, submit, delivered, starting>>

\* The renderer must report the full text was presented before the review interval starts.
PreviewReturns(shown) == /\ phase = "preview"
                         /\ phase' = IF cancelPending \/ ~shown THEN "finish" ELSE "confirm"
                         /\ result' = IF cancelPending THEN "cancelled"
                                       ELSE IF shown THEN "none" ELSE "failed"
                         /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                        stopRequested, cancelPending, cueDone, cueAccepted,
                                        closeDone, writing, cleaning, submit, delivered, starting>>

ConfirmReturns(stillShown) ==
    /\ phase = "confirm" /\ phase' = "finish"
    /\ result' = IF cancelPending THEN "cancelled"
                  ELSE IF stillShown THEN "accepted" ELSE "failed"
    /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                   stopRequested, cancelPending, cueDone, cueAccepted,
                   closeDone, writing, cleaning, submit, delivered, starting>>

\* An explicit write. It never continues into dictation cleanup: cleanup is the other path.
WritingReturns == /\ phase = "writing" /\ phase' = "finish"
                  /\ result' = IF cancelPending THEN "cancelled" ELSE "accepted"
                  /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                                 stopRequested, cancelPending, cueDone, cueAccepted,
                                 closeDone, writing, cleaning, submit, delivered, starting>>

\* Last empty try_recv is the admission fence; later sends belong to the next flow. Settling
\* delivers an outcome only if no ending control was consumed anywhere in drive.
FinishEmpty == /\ phase = "finish" /\ queue = <<>> /\ phase' = "done"
               /\ delivered' = (result = "accepted"
                                /\ (DropCompletionGuard \/ ~ended))
               /\ UNCHANGED <<queue, sent, off, quit, preEnds, ended, intentOff,
                              stopRequested, cancelPending, cueDone, cueAccepted,
                              closeDone, writing, cleaning, submit, result, starting>>

Next == (\E c \in Commands : Send(c)) \/ DrainOne \/ PreEmpty \/ Receive
        \/ Loaded \/ LoadFailed \/ Prepare \/ CleanupReturns \/ Accept \/ ActiveCancelled
        \/ CueReturns \/ CloseReturns
        \/ WritingReturns \/ FinishEmpty
        \/ (\E shown \in BOOLEAN : PreviewReturns(shown) \/ ConfirmReturns(shown))
Spec == Init /\ [][Next]_vars
\* The shape of every variable; the chart command checks only this.
TypeOK == /\ phase \in {"pre", "active", "cleaning", "ending", "preview", "confirm", "writing", "finish", "done"}
          /\ queue \in Seq(Commands) /\ Len(queue) <= MaxControls /\ sent \in 0..MaxControls
          /\ \A b \in {off, quit, preEnds, ended, intentOff, stopRequested, cancelPending, cueDone,
                       cueAccepted, closeDone, writing, cleaning, submit, delivered, starting} : b \in BOOLEAN
          /\ result \in {"none", "failed", "cancelled", "accepted"}
NoDeliveryAfterStop == delivered => ~stopRequested
\* Nothing is accepted while the model is still loading.
AcceptedOnlyAfterLoad == phase \in {"cleaning", "ending", "preview", "confirm", "writing"} => ~starting
=============================================================================
