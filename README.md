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
Qwen3-TTS, as a systemd user service. With Kokoro running (`docker compose
--profile kokoro up -d --wait`), one script needs no root and no Docker: it
builds the server from the pinned commit of
[a fork](https://github.com/NotPellew/qwentts.cpp) that adds Thalvi's
streaming route and startup voice (offered upstream in
[#45](https://github.com/ServeurpersoCom/qwentts.cpp/pull/45)), downloads the
checksummed models, has Kokoro read the `af_heart` reference clip, and writes
the unit.

```sh
speech/qwentts.sh
systemctl --user enable --now thalvi-qwentts
```

It needs `git`, `cmake`, a C++ compiler and the CUDA toolkit (`nvcc`). It
builds for the GPU it finds; to build on a machine without one, or for
another, set `CUDA_ARCH` (`75` for an RTX 2080). `KOKORO_URL` points it at a
Kokoro on another host, and `SPEECH_DIR`, `HOST`, `PORT` (default `8011`) and
`VOICE` (default `af_heart`) change where it lives and what it serves. Rerun
it to resume after an error. The script prints the lines for `.env`:

```sh
TTS_API=qwen3
TTS_BASE_URL=http://127.0.0.1:8011/v1
TTS_VOICE=af_heart
```

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
