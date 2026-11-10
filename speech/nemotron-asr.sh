#!/usr/bin/env bash
# Installs Thalvi's live speech recognizer (NVIDIA Nemotron unified ASR 0.6B, English, German,
# Spanish and more) on an NVIDIA GPU, in user space. Run it from a Thalvi checkout; rerun to
# resume or upgrade, finished steps are skipped.
#
# Settings (environment): SPEECH_DIR, HOST, PORT, ENDPOINT_MS (silence that ends a stretch of speech).
set -euo pipefail

HF=https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b/resolve/023ce907267a09a308a5cb69239447cb3d0c1597
MODEL=nemotron-labs-unified-asr-0.6b.nemo
MODEL_SHA=1718cc1b44fb8597b2356328d706a8d89f31c389ca6f2256c2e3f743c0246936

SPEECH_DIR=${SPEECH_DIR:-$HOME/thalvi-asr}
HOST=${HOST:-127.0.0.1}
PORT=${PORT:-8012}
ENDPOINT_MS=${ENDPOINT_MS:-600}

SOURCE=$(cd "$(dirname "$0")/asr" && pwd)
APP=$SPEECH_DIR/app
MODELS=$SPEECH_DIR/models
UNIT=$HOME/.config/systemd/user/thalvi-asr.service

die() { echo "nemotron-asr.sh: $*" >&2; exit 1; }

for tool in uv curl sha256sum nvidia-smi; do
  command -v "$tool" >/dev/null || die "$tool is not installed"
done

mkdir -p "$APP" "$MODELS" "$SPEECH_DIR/tmp" "$(dirname "$UNIT")"
cp "$SOURCE"/{thalvi_asr.py,nemo_stream.py,pipeline.yaml,pyproject.toml,uv.lock,.python-version} "$APP/"

echo "Installing the recognizer and NeMo (several GB the first time)..."
uv sync --quiet --frozen --no-dev --extra nemo --project "$APP"

if ! { [ -f "$MODELS/$MODEL" ] && echo "$MODEL_SHA  $MODELS/$MODEL" | sha256sum -c --status; }; then
  echo "Downloading $MODEL (2.5 GB)..."
  curl -fL --retry 3 -C - -o "$MODELS/$MODEL" "$HF/$MODEL"
  if ! echo "$MODEL_SHA  $MODELS/$MODEL" | sha256sum -c --status; then
    rm -f "$MODELS/$MODEL"
    die "$MODEL did not match its checksum and was deleted; rerun to download it again"
  fi
fi

cat > "$UNIT" <<UNITEOF
[Unit]
Description=Thalvi live speech recognition (Nemotron unified ASR)
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
WorkingDirectory=$APP
ExecStart="$APP/.venv/bin/python" -W ignore thalvi_asr.py --host $HOST --port $PORT --model "$MODELS/$MODEL" --endpoint-ms $ENDPOINT_MS
# NeMo unpacks the 2.5 GB model into TMPDIR, which is often a small tmpfs.
Environment=TMPDIR=$SPEECH_DIR/tmp
Environment=HF_HUB_OFFLINE=1
MemoryMax=12G
MemorySwapMax=0
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
UNITEOF
systemctl --user daemon-reload 2>/dev/null || true

cat <<DONE

Installed. Start it, and at every boot, with:
  systemctl --user enable --now thalvi-asr
  loginctl enable-linger   # keeps user services running without a login session
After an upgrade, restart it: systemctl --user restart thalvi-asr
It needs about 3 GB of VRAM and is ready once it logs "listening".
Then put this in Thalvi's .env and restart the server:
  STT_URL=ws://$HOST:$PORT
  STT_MODEL=nemotron-labs-unified-asr-0.6b
  STT_API_KEY=
DONE
