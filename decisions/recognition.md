# Recognition

Turning speech into text: on-device models, speech servers, the word list, extra recognizers and the final pass.

- **Local recognition in the desktop app.** `stt.engine: local` runs a catalog model on `ort`, picked in Settings from a Language drop-down (filters only, not saved) and a Model drop-down: Nemotron Streaming (0.6B, 160 ms, default), Nemotron Unified (0.6B, buffered) or Parakeet TDT v3 (0.6B, offline, re-read every 640 ms). Why: offline; a model is listed for a real trade-off, not only for the fewest errors; word errors on
  LibriSpeech and the developer's takes 3.46/7.14% and 3.03/6.35%, Unified also 8.38% German, 7.09% Spanish; Parakeet about 0.15× real time against Unified's 1×, at 2.33/4.76% (Unified 2.25/4.76% on those clips), 12.44% German, 7.95% Spanish. Rejected: Moonshine (7.4%), Zipformer
  (28%), Nemotron 3.5 (15%), the 80 and 560 ms exports (no real choice; 3.62 and 3.46% on LibriSpeech).
- **On-device recognition stops three seconds behind the microphone.** The capture ends with the model's name once
  the oldest frame the model has not processed is 3 s old, and the metrics record `cant_keep_up` with that lag. Why:
  past 3 s the words come too late to correct what is on screen, and an unbounded backlog once left the app
  unresponsive for 15 minutes on a 4-CPU VM (#455). Rejected: carrying on late and only saying so, which keeps the backlog.
- **A can't-keep-up failure offers Parakeet TDT v3 as the lighter model.** The offer shows on the notice and in the
  tray only while the active on-device choice is Nemotron Streaming or Unified; with Parakeet, a folder, Whisper or a
  server live, the notice points to a server recognizer. Why: Parakeet costs about 0.15× real time against Unified's 1×.
  Rejected: offering every other model, or ranking them by measured speed.
- **Experimental Whisper Turbo in an app-owned whisper.cpp helper.** On this computer also offers Whisper Turbo on the CPU or a Vulkan GPU: pinned Turbo q5_0 and Silero VAD 6.2.0 (MIT) download like the catalog, and the packaged whisper.cpp d1be6fde server runs on loopback per backend, with VAD (400 ms pad) and timestamps, gets each finished capture, sends a non-empty word list as `prompt` with `vad=true` per call, and is restarted when one is cancelled mid-request. On Vulkan, text previews while the user speaks: the whole capture so far, re-read once 0.8 s of new audio has come and the last preview answered; only the final is delivered. The CPU shows none.
  Why: the app never loads Vulkan, so a missing driver fails only that choice; an empty overlay hid what was being heard (#482), and a CPU read takes as long as the speech; on 40 FLEURS clips 4.3% German, 5.0% English errors, f16 no better; without VAD silence became "Thank you.", without timestamps a sentence after 30 s vanished; a dropped request held the next one ~5 s on the CPU; per the 2026-11-23 decision to send the word list everywhere (superseding #381), on the real CPU helper the list fixed "QEN 3.8." → "Qwen, Qwen3.8" while per-call VAD kept 5 s silence and 5 s pink noise empty with a 10-entry list (VAD off echoed "Q1"; Vulkan unmeasured).
  Rejected: linking whisper.cpp into the app, one binary for both (it needs vulkan-1.dll even on the CPU), transcribe.cpp (untested), a device picker.
- **Self-hosted recognition speaks Meta's ASR protocol.** `speech/asr` serves Nemotron unified ASR
  0.6B (NeMo, buffered, 0.56 s lookahead, language detected) as a realtime speech entry. Why: the app
  already speaks it; 5.8–6.6% word errors on the developer's takes (Meta-free, en/de/es), fits on an
  8 GB RTX 2080. Rejected: Voxtral 4B Realtime (too big), Nemotron 3.5 streaming (15%), Qwen3-ASR (2 s chunks).
- **The desktop connects to its recognizer itself, from a list of speech servers.** `stt.entries` holds realtime
  ASR, OpenAI-compatible buffered transcription (request kind Transcriptions or Chat with audio, optional language), on-device and extra entries; one live entry is active, at most one other entry may be the final pass (pausing extras), and otherwise every extra switched on feeds the chat-model merge.
  Why: switch recognizers with one click, no config edit or restart, and no ranking to manage. Rejected: one setting per recognizer, a silent fallback between entries, several live entries, ranking extras, first-answer-wins races (#228).
- **A fresh install recognizes on this computer.** With no `stt.entries` the one active entry is On this computer
  with Nemotron Streaming; the first start writes the defaults, opens Settings on Recognition and downloads the model.
  Why: the packaged app ships no server. Rejected: a setup wizard, waiting for a click before downloading.
- **The self-hosted recognizer computes in fp32.** Why: in fp16 its encoder overflows to NaN, decoded as `⁇`,
  when speech follows seconds of digital silence (#220); fp32 takes 1.25 GB more VRAM (7.7 of 8 GB beside TTS)
  and about 2 ms more per step on the RTX 2080. Rejected: bfloat16 (Turing has none), filling silence with
  noise (rescued one stretch of two), restarting streams.
- **The word list goes to every transcription path whose protocol can carry it.** When `stt.phrases` is non-empty, it goes to on-device boosting, direct realtime or `phraseBoosting` servers, OpenAI-compatible uploads (active, extra, or final pass, as `, `-joined `prompt` or chat system message), Meta `/asr/transcribe` uploads (extra or final pass, as `keywords` in `request`), the Whisper Turbo helper (`prompt` with `vad=true`), and the extra-recognizer merge (exact spellings when present, still under `faithful`); empty sends nothing extra.
  Why: owner decision of 2026-11-23 so a word added in Settings works whichever path runs; text replacement can't fix what was misheard. NeMo boosting fixed "Cybrex2" with no new errors; on "Qwen. Qwen 3.8." (2026-11-23), Qwen3-ASR-1.7B on Cybrex2 went from "Quen. Quen 3.8." to "Qwen. Qwen 3.8." with `prompt`, and Meta `muse-voice-transcribe-1.0` returned "Qwen Qwen 3.8" with `keywords`.
  Rejected: a flat logit bonus (no fix, stray tokens in silence).
- **Local boosting fuses NeMo's phrase tree into the greedy search at alpha 2.** Why: "Cybrex2", digit
  and all, in 8 of 8 dictations, no new errors on 48 takes with a two-entry list, about 1 ms more per
  step. Rejected: a beam of 4 (0 of 8 at alpha 1, twice the step time), NeMo's alpha 1 (0 of 8).
- **ONNX Runtime from ort's prebuilt binaries, linked in.** On Linux its WebGPU build also needs
  `libwebgpu_dawn.so` beside the executable (rpath `$ORIGIN`, shipped in the AppImage). Why: one way
  to provide it. Rejected: a system or bundled dynamic ONNX Runtime (a version and a file per OS).
- **WebGPU for on-device recognition, discrete GPUs only.** Linux runs Nemotron Unified (fp16 export) on
  one, else on the CPU with the reason in Settings, also until its 1.3 GB are downloaded; other models and
  Windows (#317) stay on the CPU. Why: partial lag p50 73 → 32 ms at the same word errors; Streaming and
  Parakeet were no faster. Rejected: CUDA (NVIDIA only, 1.65 GiB, wrong fp16 in #282), DirectML, integrated GPUs.
- **Model files checked once, on download.** A pinned model file must match its SHA-256 as it is
  downloaded; a file already on disk counts as intact when it has the pinned size, else it is replaced.
  Why: hashing 0.6–1.9 GB on every load made switching back to a model take 11–20 s (#494).
  Rejected: hashing on every load; trusting a file of the wrong size.
- **A dictation waits for a model that is loading, not for one that is downloading.** On this computer the
  capture waits with no limit while the model is read from disk, and fails at once with the percent once it
  downloads; servers keep the 8 s limit. Why: a read takes seconds and ends, a 1.3 GB download takes minutes.
  Rejected: one longer fixed limit for both.
- **Extra recognizers are opt-in and merge only when asked.** Any recognizer can be an extra: an upload kind (`extra` entry) or a live-kind entry other than the active one (`"extra": true`), which hears the capture's frames from the start but never shows partials or decides an ending or route, and failing or lagging it never slows the live one (#339). An extra runs only when switched on (`active`) and no final pass is selected (which pauses extras and merge while keeping their flags); `stt.merge` on merges its text through `writing`, so extras need a chat endpoint then, and off delivers the live text at once and only records what the extra heard.
  Why: no upload or latency without an explicit choice, and the merge is the costly part, so it is chosen separately.
  Rejected: treating an endpoint's presence as enablement, ranking extras, first-answer-wins races (#228).
- **A final pass replaces the live text.** One chosen non-active entry (`"final_pass": true`, no chat endpoint needed; pauses extras/merge while keeping their flags) re-transcribes finished capture audio while the live recognizer keeps live words, endings and route; OpenAI-compatible uploads send a non-empty word list (`stt.phrases`, `, `-joined) as `prompt` or a system message, and Meta uploads send it as `keywords`.
  A usable answer within 2 s of release or the end of the initial spoken pause (matching ending stripped on wake; whole text on hold) replaces the live text before cleanup, submit confirmation and writing; timeout, error or disagreement uses the live text with a notice, scratch/empty/task skips it, and cancel delivers nothing (#586).
  Why: on a limited corpus of 33 hand-corrected developer dictations (767 words, LLM-Test, 2026-11-20), Qwen3-ASR-1.7B alone made 2.7% word errors (0.3 s/clip on an RTX 5070 Ti; 2.3% with `stt.phrases` context vs 2.7% without) vs 3.4% for a DeepSeek merge of live + Whisper Turbo + Qwen, 4.4% for Whisper Turbo and 5.7% for live Nemotron Unified. The comparison motivates trying one stronger pass while keeping live feedback.
  Rejected: making a non-streaming recognizer active (no live words), ranking multiple final passes or first-answer-wins races (#228), running a final pass on scratch or tasks, requiring a chat endpoint.
- **Merge settles dictation; tasks use live words.** Paste and write use the merged transcript after the ending pause. With number formatting on, Submit waits for the merge, formats and optionally cleans the text, then shows the full result for confirmation. With formatting off, Submit keeps the earlier live confirmation.
  Why: merging fixes recognition errors. A task acts on the words the user heard, and number formatting uses the transcript that will be delivered.
  Rejected: merging tasks, always using live text for Submit (#500, #588).
- **Long dictation repair.** A decoding that commits 24 frames without a token while the voice detector still hears speech
  replays the whole stretch, and the final hears the last eight seconds again from a word boundary, with the silence after it;
  only a stretch up to 9.6 s is heard whole. Why: a windowed encode's frames differ from the stretch's, so the greedy search can
  wander off and emit blanks forever (12 s lost its last sentence), and a whole-stretch final grows past the wait (620 ms at
  30 s, 1.7 s at 64 s), while its last eight seconds cost 75-180 ms and at most one word. Rejected: 32 frames of left context
  and a global mel over the capture (both stall the same way); a longer release wait; a whole-stretch final at every length.
