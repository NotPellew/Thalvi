# Thalvi

A voice assistant for the moments when typing isn't an option: hands in the
dough, eyes on the road, arms full of groceries. Talk to it, interrupt it, show
it what you're looking at, and it works through the problem with you.

- **Cooking:** show it your fridge, pick a recipe together, and get walked
  through it step by step with timers.
- **On the road:** a briefing on the topics you care about, then follow-up
  questions in plain conversation.
- **Messages on the go:** hear what came in, dictate a reply, and confirm it
  out loud before anything is sent.

Built to self-host. By default it runs on Meta's models: Muse Voice Transcribe
for live speech, Muse Spark for reasoning and tool use, SAM 3.1 for pointing at
things in the camera view. Speech runs locally: a Qwen3-TTS clone of Kokoro's
`af_heart` voice on a GPU, or Kokoro itself on CPU. Each model is an endpoint
in `.env`, so any server that speaks the same protocol can replace it: Ollama
or vLLM for the language model, for example.

## Run it

1. Start speech (details in [Speech](#speech)). On an NVIDIA GPU with 9 GB
   of free VRAM:

   ```sh
   hf download Qwen/Qwen3-TTS-12Hz-1.7B-Base --local-dir ~/thalvi-tts/models/qwen3-base
   docker compose --profile vllm-omni up -d --wait
   ```

   Without one, `docker compose --profile kokoro up -d --wait`.
2. `cp .env.example .env` and fill in the API keys and the speech lines for the
   engine you started.
3. `npm ci --prefix web && npm run build --prefix web`, then `cargo run --release`.
4. `tailscale serve --bg 8080` and open the printed HTTPS address on your phone.

## Speech

`compose.yaml` runs the speech server in Docker, with pinned images, ports on
127.0.0.1 and memory limits. Weights and registered voices live under
`$SPEECH_DIR` (default `~/thalvi-tts`). `--wait` returns once the server answers
its health check. Thalvi reads `TTS_API`, `TTS_BASE_URL`, `TTS_VOICE` and, for
`openai`, `TTS_MODEL` from `.env`; to switch engine, change those lines and
restart Thalvi.

| Engine | First audio | Cost | Needs |
| --- | --- | --- | --- |
| Kokoro | ~0.15 s for a short sentence, ~1 s for a long one | CPU, ~1 GB RAM | nothing |
| Qwen3-TTS clone of `af_heart`, vLLM-Omni | ~50 ms | ~8 GB VRAM, ~8 GB RAM, ~2.5 min to start | NVIDIA GPU, Ampere or newer |
| Qwen3-TTS clone of `af_heart`, qwentts.cpp | ~60 ms (RTX 2080), ~4.8× real time | ~4 GB VRAM, ~3 GB RAM, starts in seconds | NVIDIA GPU, including pre-Ampere, and the CUDA toolkit to build |

**Kokoro** (`--profile kokoro`):

```sh
TTS_API=openai
TTS_BASE_URL=http://127.0.0.1:8880/v1
TTS_MODEL=kokoro
TTS_VOICE=af_heart
```

Any [Kokoro voice](https://huggingface.co/hexgrad/Kokoro-82M/blob/main/VOICES.md)
name works for `TTS_VOICE`.

**Qwen3-TTS on vLLM-Omni** (`--profile vllm-omni`). The first start also runs
Kokoro, which reads a reference line for the `af_heart` clone and then stays up
as the CPU fallback.

```sh
TTS_API=openai
TTS_BASE_URL=http://127.0.0.1:8091/v1
TTS_MODEL=Qwen/Qwen3-TTS-12Hz-1.7B-Base
TTS_VOICE=af_heart
```

To speak in another voice, register a clip of it (a few seconds of clean
speech) with its transcript, then put the name in `TTS_VOICE`:

```sh
curl 127.0.0.1:8091/v1/audio/voices -F audio_sample=@me.wav -F consent=mine \
  -F name=me -F "ref_text=What the clip says"
```

A misspelled voice name fails with a confusing "does not support
task_type='CustomVoice'" error. Qwen3's built-in presets (such as `vivian`)
are not in the Base checkpoint; they need `Qwen/Qwen3-TTS-12Hz-1.7B-CustomVoice`
in its place (download it, then change the model path and `--served-model-name`
in `compose.yaml` and `TTS_MODEL`).

**Qwen3-TTS on older GPUs** (before Ampere, such as an RTX 2080, which
vLLM-Omni does not run on, but any NVIDIA GPU works) runs
[qwentts.cpp](https://github.com/ServeurpersoCom/qwentts.cpp), a C++ port of
Qwen3-TTS, as a systemd user service. The script needs no root: it builds the
server from a pinned commit of [a fork](https://github.com/NotPellew/qwentts.cpp)
that adds Thalvi's framed streaming route (offered upstream in
[#45](https://github.com/ServeurpersoCom/qwentts.cpp/pull/45)) and a startup
voice, downloads the checksummed models, has Kokoro read the `af_heart`
reference clip once, and writes the unit.

```sh
docker compose --profile kokoro up -d --wait   # or set KOKORO_URL to a running Kokoro
speech/qwentts.sh
systemctl --user enable --now thalvi-qwentts
```

It needs `git`, `cmake`, a C++ compiler and the CUDA toolkit (`nvcc` on
`PATH`). Set `CUDA_ARCH` to build for a GPU other than the one ggml detects
(`75` for an RTX 2080); `SPEECH_DIR`, `HOST` and `PORT` (default `8011`) change
where it lives and listens. The server has no authentication, so a `HOST` other
than `127.0.0.1` exposes it to that network. Rerun the script to resume after an
error or to upgrade, then restart the service. It prints the lines for `.env`:

```sh
TTS_API=qwen3
TTS_BASE_URL=http://127.0.0.1:8011/v1
TTS_VOICE=af_heart
```

**Choosing a voice in the app.** `TTS_VOICES` lists the voices the app offers,
comma-separated; `TTS_VOICE` stays the default and is always offered first.
Names must be ones the running engine knows (see above). Tap **Voice** in the
header to open the picker, which starts closed. **Preview** speaks a short
phrase in the selected voice, only when tapped, and plays it on that device;
it neither interrupts a reply nor turns voice on. **Apply** saves the choice in
that browser only (other devices keep their own) and sends it to the live
conversation, and again each time the connection reopens. A saved voice the
server no longer offers falls back to `TTS_VOICE`. Previews are timed in the
metrics (`voice_preview` events, `THALVI_RECORD=metrics`).

```sh
TTS_VOICE=af_heart
TTS_VOICES=af_heart,af_bella,sohee
```

## Audio input

For local wake words, dictation into a desktop window, and scripted computer
actions, see the [native desktop client](desktop/README.md). The first target
is Hyprland; phone use remains conversation-only.

With `LLM_AUDIO_INPUT=on` the language model hears a spoken turn instead of
reading its transcript: the turn's audio, as recorded and padded by 300 ms on
each side, is sent as an OpenAI `input_audio` WAV part with no text beside it.
The model must accept such parts; the transcript still drives the captions,
the saved conversation and earlier turns, which go to the model as text. Typed
turns are always text. Recordings of replies (`THALVI_RECORD=metrics`) carry
`input_audio_ms`, the length of the clip sent.

## Conversations

Every conversation is kept under `$THALVI_DATA_DIR/sessions/` (default
`data/sessions/`, one JSON file each) until you delete it from the list in the
app. A reload or a dropped connection rejoins the same conversation, and
opening one that is live on another device moves it there. There is no
authentication: anyone who can reach the server can list, read and delete
every conversation, so keep it on your tailnet.

Titles start as the first line you said. Set all three of `BG_LLM_BASE_URL`,
`BG_LLM_MODEL` and `BG_LLM_API_KEY` (any Chat Completions endpoint, set
together or not at all) and a background model rewrites each title in three to
six words after the first reply, off the reply path. A title you typed is
never overwritten.

Thalvi can use the read-only `search_history` tool to find short, timestamped
keyword matches in the current and past conversations. Tool results are data,
not instructions. The app shows **Working** while a search runs; its optional
**Task steps** section starts collapsed and opens only when you tap it.

With `THALVI_RETIRE_DAYS=90`, inactive conversations older than 90 days are
replaced by a model-written summary on startup and then once a day. Durable
preferences and user facts are saved separately in
`$THALVI_DATA_DIR/learnings.json`; open **Conversations**, then **Learnings**,
to read or edit them. A failed model call or write leaves the full transcript
untouched. Unset or `off` disables retirement completely, so no session is
changed or deleted automatically.

## Measure a setup

`cargo run --release --bin measure -- --setup data/measure/a.env --setup data/measure/b.env`
times and grades the same prompts on each setup and prints their numbers side
by side. A setup is your normal `.env` plus an override file of env lines that
win over it; keep override files under `data/` (git-ignored), since they may
hold keys:

```sh
# data/measure/spark12.env
LLM_MODEL=muse-spark-1.2
LLM_REASONING_EFFORT=low
```

Setups run interleaved (case 1 on A, B, A, B, then case 2), so they see the
same network noise. The prompts are `crates/thalvi-server/measure/cases.json`
(greetings, facts, arithmetic, how-tos, explanations, requested lengths, a
German question, a follow-up); a multi-turn case lists alternating user and
assistant turns, ending on the user turn to answer. Flags: `--setup <file>`
(repeat; none measures the plain `.env`), `--runs <n>` (default 3), `--cases
<file>`, `--only <id>` (repeat) and `--no-judge`.

Each reply goes through the code Thalvi uses: the chat model, the sentence
splitter, the speech client from config and `SYSTEM_PROMPT`. The table gives
median (min-max) seconds from the request start to the first content token, the
first sentence, the first audio byte and the last audio byte of the whole reply,
plus sentences and audio seconds per reply, failed replies and the judge's mean
score. The wait the user perceives adds endpointing (about 0.5-0.75 s). A reply
that hits an error or takes over 120 s counts as an error. Every reply, grade
and error is written to `data/measure/run-<time>.json`.

The judge grades each answer from 1 to 5 against a short rubric (correct, length
fits the question, speakable) after all timing is done. Set `JUDGE_BASE_URL`,
`JUDGE_MODEL` and `JUDGE_API_KEY` for any OpenAI-compatible model; without them
quality is reported as "not judged". One judge on a fixed prompt set is a
yardstick for comparing setups, not a verdict on one reply.

## Status

Under construction for the
[Meta Global AI Developer Hackathon](https://cerebralvalley.ai/e/meta-ai-developer-hackathon)
(October 26 – November 4, 2026).

## Hackathon provenance

All code in this repository was written during the hackathon; the commit
history is the record. Pull requests land as merge commits, so the
individual commits behind each change stay visible, and `main` is never
rewritten.

The idea draws on lessons from an earlier, private voice-assistant prototype
by the same author. Nothing is copied from it or from any other repository;
third-party code enters only as declared dependencies.

AI assistance is used in this project and disclosed: Claude Code orchestrates
design, implementation, and review, with other models (such as Meta's Muse)
as workers. Commits made with an agent carry its `Co-Authored-By` trailer.

## License

[Apache License 2.0](LICENSE).
