# Voice picker and speed

The Voice chip opens a picker, closed at start, where the user previews
another voice on their own device and applies it to replies in this browser.
The speed chip sets reply playback speed, which persists across reloads.

## Sub-features

- `voice-list` lists the offered voices, with the applied one marked
  `(in use)`.
- `voice-preview` speaks the preview phrase in the selected voice only when
  tapped. The button reads `Loading…`, then `Stop`, then `Preview` again.
- `voice-apply` saves the choice in this browser and uses it for the next
  spoken reply.
- `speed-set` picks one of eight speeds; the chip and storage follow.

## How to get to it (user POV)

- Choose `Voice` in the header, pick a voice, then `Preview` or `Apply`.
- Choose the speed chip (`1×`), then a speed such as `1.5×`.

## Driving it with thalvi-verify

Preconditions:

- Baseline from `features/README.md`, page at `idle`, voice off, nothing in
  `localStorage`.

Steps:

- **Open picker.** Choose `Voice`.
  - Run `$V ab $R click 'button.o-voice-chip'`,
    `$V ab $R wait --fn 'document.querySelectorAll("select[aria-label=\"Voice to try\"] option").length === 2'`,
    then `$V ab $R snapshot -i -c -s '#o-vp'`.
  - The combobox `Voice to try` offers `fake_heart (in use)` and `fake_bella`.
  - `Apply` is disabled.
- **Preview.** Select `fake_bella` and choose `Preview`.
  - Run `$V ab $R select 'select[aria-label="Voice to try"]' fake_bella` and
    `$V ab $R click '.o-vp-preview'`.
  - The brief `Loading…` and `Stop` labels can pass between commands. Record
    the preview flow and wait for `.o-vp-preview` to return to `Preview`;
    require the matching request and successful `voice_preview` metric below,
    and an empty `.o-vp-note` (no playback error) before Apply. The label
    alone can also pass before the async work starts.
  - `fakes.jsonl` has `tts_request` with `"voice": "fake_bella"` and
    `"input": "Hello, this is how I sound."`
  - A preview log under `R/data/recordings/` has `event: voice_preview` and
    `outcome: ok`. Voice stays off.
- **Apply.** Choose `Apply`.
  - Run `$V ab $R click '.o-vp-apply'`.
  - `.o-vp-note` reads `Using fake_bella on this device.`, and
    `localStorage["thalvi.voice"]` is `fake_bella`.
  - Capture the open picker here as `voice-applied.png`, before reloading.
- **Applied voice speaks.** Queue a turn and turn voice on.
  - Run `$V say $R "Which voice do you use now"`,
    `$V ab $R click 'button[aria-label="Start talking"]'` and
    `$V reply $R --after "Which voice do you use now"`.
  - The reply's `tts_request` lines use `"voice": "fake_bella"`.
  - Turn voice off with `$V ab $R click 'button.o-voice'`.
- **Speed.** Choose the speed chip, then `1.5×`.
  - Run `$V ab $R click '.o-speed-chip'` and
    `$V ab $R click 'button[aria-label="Playback speed 1.5×"]'`.
  - The chip's label is `Playback speed 1.5×`, and
    `localStorage["thalvi.speed"]` is `1.5`.
  - After `$V ab $R reload` and `$V state $R idle`, the chip still reads
    `1.5×`.
- **Proof.** Keep the screenshot and `snapshot -c` captured immediately after
  Apply, the `voice_preview` metric, and the `tts_request` lines for the
  preview and the reply.

## Gotchas

- The server's preview endpoint rejects a voice outside the run's offered
  list before calling speech. The fake speech engine itself knows only
  `fake_heart` and `fake_bella` and answers HTTP 400 for other names.
- Preview audio plays to a null sink in headless Chromium. `Stop` means the
  UI entered its playing state and attempted playback; `audio.play()` can
  still fail. A successful preview metric proves synthesis, not client playback or audibility.
- `localStorage` belongs to the page's origin, and each run has its own app
  port and browser session, so a new run starts empty. Persistence is proved
  by `reload` within one run.
- This recipe proves speed selection and persistence, not audible stretching
  quality. Speed changes the playback rate on the client only. The server and
  `tts_request` lines do not change.
