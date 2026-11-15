# Synthetic audio verification

Use synthetic input and silent output for unattended verification. Physical
audio is excluded until the developer explicitly confirms a suitable setup
exists. Do not schedule a headset session, propose buying a fixture as a task
dependency, or make human listening a completion requirement. Describe hardware
and subjective quality as outside the scope, not as unfinished synthetic tests.

## Coverage and evidence

| Layer | Drive | Required evidence |
| --- | --- | --- |
| Browser voice flow | Fake capture device and scripted ASR, normal Voice/Stop controls | Caption, reply, cancellation/barge-in metrics, ended tracks; [voice recipe](features/voice-conversation.md) |
| Real recognition | Fixed generated speech through the isolated desktop microphone or Windows file feed | Model/build identity, fixture hash, transcript, destination/clipboard and cancellation; [desktop](desktop.md), [Windows](windows.md) |
| Voice selection | Preview, Stop, candidate change, close, removed saved voice | Real media-element state, selected fallback and actual reply TTS request |
| Acquisition failures | Reject or hold `getUserMedia` in the owned page | Notice/recovery; no surviving track/context or frames after conversation termination |
| Digital playback | Shipped worklet, generated PCM, silent monitor node | Output duration, amplitude, pitch, flush and subsequent playback |
| Endpoint failures | Run-owned fake ASR/TTS/model faults | Browser outcome, request/disconnect metrics and successful subsequent turn |
| Private recording | Synthetic ordinary/private voice controls with audio recording enabled | Ordinary WAV exists; private session creates no WAV or text under app data, but retains metrics |

Fakes establish control flow. Generated speech through real models establishes
recognition on that corpus. Digital samples establish playback processing.
None establishes physical microphone/headphone operation or perceived quality.
Paid model APIs are not part of this verification.

Use the managed launch/cleanup procedure in [SKILL.md](SKILL.md). Keep all
fixtures, drivers, screenshots and metrics outside the repository. Record the
source commit, binary hash, endpoint mode and every injected fault. Retain failed
harness attempts separately from corrected runs; do not label driver errors as
product failures. Stop every owned process and verify its ports are closed.

## Browser controls

Use the same run's browser session and fake microphone flags as `$V ab`, with
`--mute-audio` added from its first launch through teardown. Keep those flags
constant: changing them restarts the browser. No physical capture is needed.
Reload between injected cases so one monkeypatch cannot affect the next case.

### Permission denial and delayed acquisition

Temporarily replace `navigator.mediaDevices.getUserMedia` with a promise that
rejects with `new DOMException("synthetic denial", "NotAllowedError")`. Click
Voice and require the permission notice and Idle. Restore the bound original,
click Voice again, require Listening, then turn it off and require ended tracks.

For a pending acquisition, install this before clicking Voice:

```js
const gum = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
window.testTracks = [];
window.testPending = false;
navigator.mediaDevices.getUserMedia = async (...args) => {
  window.testPending = true;
  await new Promise(resolve => { window.releaseTestMic = resolve; });
  const stream = await gum(...args);
  window.testTracks.push(...stream.getTracks());
  return stream;
};
```

Observe Starting and `testPending`, then exercise each actual ending separately:

- Private: close that page's captured session socket.
- Deleted: create a nonempty ordinary conversation, then DELETE its ID through
  `/sessions/<id>` while the microphone is pending.
- Moved: create a nonempty ordinary conversation, then attach another socket to
  `/session?resume=<id>` while the original microphone is pending.

Wait for Ended before calling `releaseTestMic()`. Require every acquired track
to become `ended`, all audio contexts created by that attempt to close, Voice
to remain off and zero binary frames sent by that page. Observe context creation
and WebSocket sends with wrappers that call the original methods. Close the
second socket after takeover. These are real browser resource checks with
controlled permission timing, not a physical device-disconnect simulation.

### Preview cancellation and fallback

Track the actual `Audio` elements the preview creates. For this control test,
set each fixture element's `playbackRate` to 0.25 to make the short fake preview
observable. Require `paused === false` and `currentTime > 0` before each action:
click Stop, change candidate, and close the picker. Each must pause the element
and return the preview label to Preview. Do not infer playback from the label
alone. Reload to remove instrumentation afterwards.

Set `localStorage["thalvi.voice"]` to a name absent from `/voices`, then reload.
Require the server default to be selected and marked in use. Enable Speak
replies, send a unique typed turn and require its actual `tts_request` to use
the fallback voice. This also checks that the choice reached the server.

## Digital playback

Load the built `playback-worklet-*.js` from this run's web assets into a real
browser AudioContext. Connect its `playback-stretcher` node to a monitoring
AudioWorklet that consumes input, records sample statistics and leaves its
output zero-filled; connect that silent output to the destination. Keep browser
output muted as well. Do not replace the product processor with a test copy.

Send a 24 kHz, two-second, 440 Hz sine fixture at amplitude 0.3 through the
processor's `push` message, using a fresh sequence number for each run. Exercise
rates 0.75, 1, 1.5 and 2. Wait for both the matching drain notification and the
monitor's trailing silence. Measure first/last sample above 0.001, peak amplitude
and positive zero crossings. Require duration within 80 ms of `2 / rate`, pitch
within 20 Hz of 440 Hz, and peak between 0.25 and 0.5. This checks the digital
duration/pitch contract on a tone, not speech stretching quality.

For flush, enqueue ten seconds, wait for output to begin, then send `flush` with
a newer sequence. Require the measured output to end well before the fixture's
natural end and the monitor to observe silence. Enqueue a fresh half-second
fixture and require its matching drain and duration within 50 ms. A total played
duration is not a cancellation-latency measurement: that requires a common clock
for the flush request and last nonzero output sample.

## Endpoint faults and recording isolation

Use a task-owned adapter around the existing fake endpoints. Keep it off all
resident services; retain the adapter with the run evidence. The adapter changes
only the selected fault and otherwise calls the existing fake implementation.

- After a recognizer handshake, close once with WebSocket code 1011. Capture the
  transient `recognizer_retrying` message, require a new connection, Listening
  and a successful subsequent synthetic voice turn.
- Close once with code 1008. Require the non-retrying notice, Voice off, Idle and
  ended browser tracks. Explicitly restart Voice and require another successful
  turn. A momentary socket count alone does not prove permanent no-retry.
- Return HTTP 503 from speech synthesis. Require the text reply and visible
  speech error to settle, remove the fault, then require a new reply's successful
  TTS metrics. Do not accept a request alone as recovery evidence.
- Hold a model request without headers. Require the production read timeout,
  visible notice and timeout metric, then remove the hold and require a fresh
  successful turn. Bound the fixture hold and detect client disconnect; never
  wait indefinitely or shorten the product timeout to pass.
- Start the isolated server with `THALVI_RECORD=metrics,text,audio`. Produce an
  ordinary synthetic voice turn and require a nonempty WAV as the positive
  control. Then produce a unique private voice turn, end it, and require unchanged
  WAV inventory, no private marker anywhere under app data, and a private metrics
  file. The fake endpoint log intentionally retains test messages; it is outside
  this storage assertion and must not be described as private-data-free.

Run the existing browser audio tests as a complementary regression check:

```sh
npm --prefix web test -- src/device-audio.test.ts src/mic.test.ts \
  src/playback.test.ts src/playback-worklet.test.ts src/stretch.test.ts \
  src/voice.test.ts
```

These tests include controlled browser/processor fakes. They complement the
live browser and real-model runs; do not report them as native hardware use.
For synthetic server acknowledgement, audio-echo/tool guards, streaming and
private-disconnect coverage, run `cargo test --locked -p thalvi-server --lib
session::tests` in a managed build job. Those tests call no paid provider.
