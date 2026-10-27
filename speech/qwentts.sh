#!/usr/bin/env bash
# Installs the qwentts.cpp speech server (Qwen3-TTS af_heart clone) for NVIDIA
# GPUs vLLM-Omni can't use, in user space. Rerun to resume; finished steps are skipped.
#
# Settings (environment): SPEECH_DIR, KOKORO_URL, CUDA_ARCH (CMake architecture list,
# e.g. 75 for an RTX 2080; default: this machine's GPU), HOST, PORT, VOICE.
set -euo pipefail

REPO=https://github.com/NotPellew/qwentts.cpp.git
COMMIT=d8782897e3d50a700be52623002a1c72be19fc27
HF=https://huggingface.co/Serveurperso/Qwen3-TTS-GGUF/resolve/b7ee2e8c7459c3bea99da23e3d178125a7d1713c
TALKER=qwen-talker-1.7b-base-Q8_0.gguf
TALKER_SHA=4b9a33a236908dd9435a42f7a396e38038329d053b704342a6413c08544c4fda
CODEC=qwen-tokenizer-12hz-Q8_0.gguf
CODEC_SHA=1883beeed99348fc35e23dd225e9082f93f6f8c109330a33d935baa8acdbfd94
# The same line the vllm-omni profile in compose.yaml has Kokoro read.
REF_TEXT="Wait, you did what? That's amazing! Okay, let me explain how it works. First you pick a quiet spot, then you read the page out loud, nice and slowly, and it picks up the rest. Does that make sense? If it doesn't, no worries at all, we can go through it together, one step at a time."

SPEECH_DIR=${SPEECH_DIR:-$HOME/thalvi-tts}
KOKORO_URL=${KOKORO_URL:-http://127.0.0.1:8880}
CUDA_ARCH=${CUDA_ARCH:-native}
HOST=${HOST:-127.0.0.1}
PORT=${PORT:-8011}
VOICE=${VOICE:-af_heart}

SRC=$SPEECH_DIR/qwentts.cpp
MODELS=$SPEECH_DIR/models/qwentts
REF=$SPEECH_DIR/voices/$VOICE
UNIT=$HOME/.config/systemd/user/thalvi-qwentts.service

die() { echo "qwentts.sh: $*" >&2; exit 1; }

for tool in git cmake nvcc curl sha256sum; do
  command -v "$tool" >/dev/null || die "$tool is not installed (nvcc comes with the CUDA toolkit)"
done

mkdir -p "$MODELS" "$SPEECH_DIR/voices" "$(dirname "$UNIT")"

if [ ! -d "$SRC/.git" ]; then
  git clone --recursive --quiet "$REPO" "$SRC"
fi
git -C "$SRC" checkout --quiet "$COMMIT"
git -C "$SRC" submodule update --init --recursive --quiet

if [ ! -x "$SRC/build/tts-server" ] || [ ! -f "$SRC/build/.built-$COMMIT-$CUDA_ARCH" ]; then
  echo "Building tts-server for CUDA architecture '$CUDA_ARCH' (a few minutes)..."
  cmake -S "$SRC" -B "$SRC/build" -DCMAKE_BUILD_TYPE=Release -DGGML_CUDA=ON -DGGML_CUDA_FA=ON \
    -DCMAKE_CUDA_ARCHITECTURES="$CUDA_ARCH" -DCMAKE_CUDA_COMPILER="$(command -v nvcc)" >/dev/null
  cmake --build "$SRC/build" --config Release --target tts-server -j "$(nproc)"
  touch "$SRC/build/.built-$COMMIT-$CUDA_ARCH"
fi

fetch() { # file sha256
  if ! { [ -f "$MODELS/$1" ] && echo "$2  $MODELS/$1" | sha256sum -c --status; }; then
    echo "Downloading $1..."
    curl -fL --retry 3 -C - -o "$MODELS/$1" "$HF/$1"
    echo "$2  $MODELS/$1" | sha256sum -c --status || die "$1 does not match its checksum; delete it and rerun"
  fi
}
fetch "$TALKER" "$TALKER_SHA"
fetch "$CODEC" "$CODEC_SHA"

if [ ! -f "$REF.wav" ]; then
  echo "Rendering the reference clip with Kokoro at $KOKORO_URL..."
  body=$(printf '{"model":"kokoro","voice":"af_heart","response_format":"wav","input":"%s"}' "$REF_TEXT")
  curl -sf "$KOKORO_URL/v1/audio/speech" -H 'content-type: application/json' -d "$body" -o "$REF.wav.part" \
    || die "Kokoro did not answer at $KOKORO_URL; start it (docker compose --profile kokoro up -d --wait) or set KOKORO_URL"
  mv "$REF.wav.part" "$REF.wav"
fi
printf '%s' "$REF_TEXT" > "$REF.txt"

cat > "$UNIT" <<UNITEOF
[Unit]
Description=Thalvi speech (qwentts.cpp)
After=network-online.target

[Service]
ExecStart=$SRC/build/tts-server --model $MODELS/$TALKER --codec $MODELS/$CODEC --host $HOST --port $PORT --alias qwen3-base --voice-name $VOICE --voice-wav $REF.wav --voice-text $REF.txt
MemoryMax=12G
MemorySwapMax=0
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
UNITEOF
systemctl --user daemon-reload 2>/dev/null || true

cat <<DONE

Installed. Start it (and on every login) with:
  systemctl --user enable --now thalvi-qwentts
It needs about 4 GB of VRAM and is ready once it logs the voice as warmed.
Then put this in Thalvi's .env:
  TTS_API=qwen3
  TTS_BASE_URL=http://$HOST:$PORT/v1
  TTS_VOICE=$VOICE
DONE
