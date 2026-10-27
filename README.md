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
things in the camera view. Speech comes from Kokoro, running locally. Each
model is an endpoint in `.env`, so any server that speaks the same protocol can
replace it: Ollama or vLLM for the language model, for example.

## Run it

1. Start Kokoro: `docker run -d -p 127.0.0.1:8880:8880 ghcr.io/remsky/kokoro-fastapi-cpu:latest`.
2. `cp .env.example .env` and fill in the API keys.
3. `npm ci --prefix web && npm run build --prefix web`, then `cargo run --release`.
4. `tailscale serve --bg 8080` and open the printed HTTPS address on your phone.

## Train the audio adapter

`train/` teaches Muse Glimmer to hear. Run everything from `train/` after
`uv sync --extra cpu`; the Modal steps need no GPU locally.

1. Local data: `BASETEN_API_KEY=… uv run python -m thalvi_train.questions ~/datasets/thalvi/questions.jsonl`,
   then, with Kokoro running, `uv run python -m thalvi_train.speak ~/datasets/thalvi/questions.jsonl ~/datasets/thalvi/speech`.
2. Log in to Modal once: `uvx modal setup`.
3. Fetch the weights and LibriSpeech inside Modal:
   `uv run modal run src/thalvi_train/modal_app.py::glimmer` and `::librispeech`.
4. Upload the Kokoro speech as one tar, since the volume serves small files slowly:
   `tar cf speech.tar -C ~/datasets/thalvi speech`, then
   `uv run modal volume put thalvi-data speech.tar /speech.tar`.
5. Run `::teacher`, then `::train` (same prefix). Each prints its start, end and hours
   on the GPU. Training and evaluation stage the audio onto the container's local
   disk first (LibriSpeech is downloaded again, ~7 GB) and run the Whisper encoder
   inline. Use `modal run --detach` for the long ones, and `--limit 50` to `::teacher`
   for a first look at the answers.
6. Score a run on the held-out eval split: `uv run modal run src/thalvi_train/modal_app.py::evaluate --run-name <run-name>`
   (`--limit 500` and `--batch-size 32` by default). It reports KL, the word error rate
   of the model's own transcription, batched decoding throughput, and the latency of a
   single request with and without the encoder.

Outputs stay on the `thalvi-data` volume: `teacher/train.jsonl`,
`runs/<run-name>/projector.safetensors`, `runs/<run-name>/eval.jsonl` and `eval.summary.json`.

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
