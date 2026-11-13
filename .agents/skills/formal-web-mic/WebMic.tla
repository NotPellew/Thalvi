--------------------------------- MODULE WebMic ---------------------------------
(* The web client's microphone around a conversation's end, as web/src/device-audio.ts
   (startVoice, acquire, releaseUnused), web/src/mic.ts (openMic) and
   web/src/controller.ts (setVoice, micStopped, message) run it. JavaScript runs
   one task at a time, so each action is the code between two awaits, or one
   event from the server, the user or the device. SKILL.md maps them. *)
EXTENDS Naturals

CONSTANTS MaxTaps,
          ClosedAddModuleRejects,  \* measured: Chromium 153 resolves addModule on a closed context
          ClosedNodeThrows,        \* measured: Chromium 153 throws InvalidStateError for the node
          OpenMicLeaksTracks,      \* mutation: openMic stops no tracks when it fails after getUserMedia
          KeepsMicAfterEnd,        \* mutation: startVoice keeps a microphone that arrives after the link ended
          ReleaseKeepsMic, MicStoppedKeepsVoice

Ctxs == 1..MaxTaps
Streams == 1..MaxTaps

VARIABLES
    link,       \* controller.state.link: "online" or "ended"
    voice,      \* controller.state.voice (speak follows it in setVoice)
    audio,      \* main.ts `audio`: the context in use, or 0
    ctxState,   \* each AudioContext: "unmade", "open" or "closed"
    pc,         \* startVoice's await in progress: "idle", "resume", "gum", "addModule"
    startCtx,   \* the context the running startVoice uses
    stream,     \* each getUserMedia stream: "none", "live" or "stopped"
    owner,      \* who can stop each stream: "inflight", "micInput" or "nobody"
    micInput,   \* main.ts `micInput`: the stream it holds, or 0
    taps

vars == <<link, voice, audio, ctxState, pc, startCtx, stream, owner, micInput, taps>>

Init ==
    /\ link = "online"
    /\ voice = FALSE
    /\ audio = 0
    /\ ctxState = [c \in Ctxs |-> "unmade"]
    /\ pc = "idle"
    /\ startCtx = 0
    /\ stream = [s \in Streams |-> "none"]
    /\ owner = [s \in Streams |-> "nobody"]
    /\ micInput = 0
    /\ taps = 0

Busy == pc # "idle"

\* releaseUnused: stop the mic unless voice is on; close the context unless voice or speak is.
Release(v) ==
    /\ IF ~v /\ micInput # 0 /\ ~ReleaseKeepsMic
       THEN /\ stream' = [stream EXCEPT ![micInput] = "stopped"]
            /\ micInput' = 0
       ELSE UNCHANGED <<stream, micInput>>
    /\ IF ~v /\ audio # 0
       THEN /\ ctxState' = [ctxState EXCEPT ![audio] = "closed"]
            /\ audio' = 0
       ELSE UNCHANGED <<ctxState, audio>>

\* view.onVoice -> startVoice: the Voice button works while nothing is busy and the link has not ended.
Tap ==
    /\ pc = "idle" /\ link # "ended" /\ ~voice /\ taps < MaxTaps
    /\ taps' = taps + 1
    /\ IF audio # 0
       THEN /\ startCtx' = audio /\ pc' = "gum"
            /\ UNCHANGED ctxState
       ELSE /\ startCtx' = taps + 1
            /\ ctxState' = [ctxState EXCEPT ![taps + 1] = "open"]
            /\ pc' = "resume"
    /\ UNCHANGED <<link, voice, audio, stream, owner, micInput>>

\* acquireAudio: `await ctx.resume()` returns; `audio` is set and openMic starts getUserMedia.
Resumed ==
    /\ pc = "resume"
    /\ audio' = startCtx
    /\ pc' = "gum"
    /\ UNCHANGED <<link, voice, ctxState, startCtx, stream, owner, micInput, taps>>

\* The startVoice catch: releaseUnused, a notice, busy cleared.
Failed ==
    /\ pc' = "idle"
    /\ Release(voice)

\* The same catch after openMic got a stream: this tap's stream ends up `st`, and nobody's.
FailWith(st) ==
    LET base == [stream EXCEPT ![taps] = st]
        stopMic == ~voice /\ micInput # 0 /\ ~ReleaseKeepsMic
    IN /\ stream' = IF stopMic THEN [base EXCEPT ![micInput] = "stopped"] ELSE base
       /\ micInput' = IF stopMic THEN 0 ELSE micInput
       /\ owner' = [owner EXCEPT ![taps] = "nobody"]
       /\ pc' = "idle"
       /\ IF ~voice /\ audio # 0
          THEN ctxState' = [ctxState EXCEPT ![audio] = "closed"] /\ audio' = 0
          ELSE UNCHANGED <<ctxState, audio>>

\* openMic: getUserMedia resolves with a live stream, or rejects (denied, no device).
Granted ==
    /\ pc = "gum"
    /\ stream' = [stream EXCEPT ![taps] = "live"]
    /\ owner' = [owner EXCEPT ![taps] = "inflight"]
    /\ pc' = "addModule"
    /\ UNCHANGED <<link, voice, audio, ctxState, startCtx, micInput, taps>>

Denied ==
    /\ pc = "gum"
    /\ Failed
    /\ UNCHANGED <<link, voice, startCtx, owner, taps>>

\* openMic: `await ctx.audioWorklet.addModule(...)` settles. With no await after it, openMic
\* builds its nodes and returns, and startVoice sets micInput and calls setVoice(true), all in
\* one step. A node on a closed context throws; openMic's catch stops the tracks. startVoice
\* stops a microphone that arrives after the link ended and calls releaseUnused.
Opened ==
    /\ pc = "addModule"
    /\ IF ctxState[startCtx] = "closed" /\ ClosedAddModuleRejects
       THEN FailWith("stopped") /\ UNCHANGED voice
       ELSE IF ctxState[startCtx] = "closed" /\ ClosedNodeThrows
       THEN FailWith(IF OpenMicLeaksTracks THEN "live" ELSE "stopped") /\ UNCHANGED voice
       ELSE IF link = "ended" /\ ~KeepsMicAfterEnd
       THEN FailWith("stopped") /\ UNCHANGED voice
       ELSE /\ micInput' = taps
            /\ owner' = [owner EXCEPT ![taps] = "micInput"]
            /\ voice' = TRUE
            /\ pc' = "idle"
            /\ UNCHANGED <<stream, ctxState, audio>>
    /\ UNCHANGED <<link, startCtx, taps>>

\* Controller.message, type "ended" (moved or deleted, or a private conversation lost):
\* voice and speak off, then releaseUnused.
Ended ==
    /\ link = "online"
    /\ link' = "ended"
    /\ voice' = FALSE
    /\ Release(FALSE)
    /\ UNCHANGED <<pc, startCtx, owner, taps>>

\* Controller.message, type "input" with source "text" while voice is on: the server turned voice off.
ServerTextInput ==
    /\ voice
    /\ voice' = FALSE
    /\ Release(FALSE)
    /\ UNCHANGED <<link, pc, startCtx, owner, taps>>

\* view.onVoice while voice is on: setVoice(false).
VoiceOff ==
    /\ voice /\ pc = "idle" /\ link # "ended"
    /\ voice' = FALSE
    /\ Release(FALSE)
    /\ UNCHANGED <<link, pc, startCtx, owner, taps>>

\* The device goes away: the track ends, and its `onended` with micInput set calls
\* controller.micStopped, which turns voice off and calls releaseUnused.
TrackEnded ==
    /\ micInput # 0 /\ stream[micInput] = "live"
    /\ stream' = [stream EXCEPT ![micInput] = "stopped"]
    /\ IF MicStoppedKeepsVoice
       THEN UNCHANGED <<voice, micInput, ctxState, audio>>
       ELSE /\ voice' = FALSE
            /\ micInput' = IF ReleaseKeepsMic THEN micInput ELSE 0
            /\ IF audio # 0
               THEN ctxState' = [ctxState EXCEPT ![audio] = "closed"] /\ audio' = 0
               ELSE UNCHANGED <<ctxState, audio>>
    /\ UNCHANGED <<link, pc, startCtx, owner, taps>>

Next ==
    \/ Tap \/ Resumed \/ Granted \/ Denied \/ Opened
    \/ Ended \/ ServerTextInput \/ VoiceOff \/ TrackEnded
    \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars

\* A live microphone always has someone who can stop it: micInput, or the openMic still running.
NoOrphanMic == \A s \in Streams : stream[s] = "live" => owner[s] \in {"inflight", "micInput"}

\* Whenever no start is running, a working microphone is held exactly while voice is on.
MicOnlyWithVoice == ~Busy => ((micInput # 0 /\ stream[micInput] = "live") = voice)

\* Once the conversation has ended and no start is running, no microphone is held.
NoMicAfterEnd == (link = "ended" /\ ~Busy) => micInput = 0
================================================================================
