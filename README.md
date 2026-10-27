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

1. Start speech, one of:
   - **Qwen3-TTS** (GPU, about 6 GB of VRAM). Download
     [Qwen3-TTS-12Hz-1.7B-Base](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-1.7B-Base)
     with `hf download Qwen/Qwen3-TTS-12Hz-1.7B-Base --local-dir ~/thalvi-tts/models/qwen3-base`,
     then start the server:

     ```sh
     docker run -d --name thalvi-qwen3 --gpus all \
       --memory=10g --memory-swap=10g --cpus=12 --pids-limit=512 \
       -p 127.0.0.1:8001:8001 \
       -v ~/thalvi-tts/models:/models:ro -v ~/thalvi-tts/voices:/data/voices \
       -e QWEN3_TTS_MODEL_ID=/models/qwen3-base -e HF_HUB_OFFLINE=1 -e HF_HOME=/tmp/hf \
       ghcr.io/malaiwah/qwen3-tts-server:latest --host 0.0.0.0 --port 8001
     ```

     Clone `af_heart` once: with Kokoro running (below), have it read a
     reference line, then register that clip and its transcript.

     ```sh
     text="Wait, you did what? That's amazing! Okay, let me explain how it works. First you pick a quiet spot, then you read the page out loud, nice and slowly, and it picks up the rest. Does that make sense? If it doesn't, no worries at all, we can go through it together, one step at a time."
     curl -s 127.0.0.1:8880/v1/audio/speech -H 'content-type: application/json' \
       -d "{\"model\":\"kokoro\",\"voice\":\"af_heart\",\"response_format\":\"wav\",\"input\":\"$text\"}" \
       -o /tmp/af_heart_ref.wav
     curl -s 127.0.0.1:8001/v1/voices -F name=af_heart -F audio=@/tmp/af_heart_ref.wav -F "ref_text=$text"
     ```

     Put the returned `id` (`vc_…`) in `TTS_VOICE`.
   - **Kokoro** (CPU; use it when the GPU is busy, for example with a local
     language model): `docker run -d -p 127.0.0.1:8880:8880 ghcr.io/remsky/kokoro-fastapi-cpu:latest`,
     and in `.env` set `TTS_API=openai`, `TTS_BASE_URL=http://127.0.0.1:8880/v1`
     and `TTS_VOICE=af_heart`.
2. `cp .env.example .env` and fill in the API keys and `TTS_VOICE`.
3. `npm ci --prefix web && npm run build --prefix web`, then `cargo run --release`.
4. `tailscale serve --bg 8080` and open the printed HTTPS address on your phone.

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
