#!/usr/bin/env python3
"""Chooses and checks the self-hosted model and speech endpoints for `thalvi-verify up --local`.

Run: python3 -I local_endpoints.py resolve REPO   prints KEY=VALUE lines for run.env
     python3 -I local_endpoints.py check ENV_FILE  probes the endpoints named in ENV_FILE

Each setting comes from the environment, else REPO/.env.verify (git-ignored), else the default.
Only hosts on loopback, a private LAN or a tailnet are accepted, so a run cannot reach a paid API.
"""

import ipaddress
import json
import os
import re
import socket
import sys
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urlparse

DEFAULTS = {
    "VERIFY_LLM_BASE_URL": "http://127.0.0.1:11434/v1",
    "VERIFY_LLM_MODEL": "qwen3:4b-instruct-2507-q4_K_M",
    "VERIFY_TTS_API": "openai",
    "VERIFY_TTS_BASE_URL": "http://127.0.0.1:8880/v1",
    "VERIFY_TTS_MODEL": "kokoro",
    "VERIFY_TTS_VOICE": "af_heart",
    "VERIFY_TTS_VOICES": "af_heart,af_bella",
}
SAFE_VALUE = re.compile(r"^[A-Za-z0-9_.,:/@+-]+$")


def fail(message: str):
    sys.exit(f"local endpoints: {message}")


def read_file(path: Path) -> dict:
    values = {}
    if path.exists():
        for line in path.read_text().splitlines():
            match = re.match(r"^\s*(VERIFY_[A-Z_]+)\s*=\s*(.*?)\s*$", line)
            if match:
                values[match.group(1)] = match.group(2).strip("\"'")
    return values


def private_host(url: str) -> bool:
    host = urlparse(url).hostname
    if not host:
        return False
    try:
        infos = socket.getaddrinfo(host, None)
    except socket.gaierror:
        fail(f"cannot resolve {host} in {url}")
    return all(not ipaddress.ip_address(info[4][0].split("%")[0]).is_global for info in infos)


def resolve(repo: Path):
    from_file = read_file(repo / ".env.verify")
    unknown = sorted(set(from_file) - set(DEFAULTS))
    if unknown:
        fail(f".env.verify sets unknown keys: {', '.join(unknown)}")
    values = {
        key: os.environ.get(key) or from_file.get(key) or value for key, value in DEFAULTS.items()
    }
    values["VERIFY_LLM_BASE_URL"] = values["VERIFY_LLM_BASE_URL"].rstrip("/")
    values["VERIFY_TTS_BASE_URL"] = values["VERIFY_TTS_BASE_URL"].rstrip("/")
    if values["VERIFY_TTS_API"] not in ("openai", "qwen3"):
        fail(f"VERIFY_TTS_API must be openai or qwen3: {values['VERIFY_TTS_API']!r}")
    for key in ("VERIFY_LLM_BASE_URL", "VERIFY_TTS_BASE_URL"):
        url = values[key]
        if urlparse(url).scheme not in ("http", "https"):
            fail(f"{key} must be an http(s) URL: {url!r}")
        if not private_host(url):
            fail(f"{key} must be on loopback, a private LAN or a tailnet, not {url}")
    for key, value in values.items():
        if not SAFE_VALUE.match(value):
            fail(f"{key} has characters run.env cannot hold: {value!r}")
    for key, value in values.items():
        print(f"{key}={value}")


def request(url: str, body: dict | None = None, timeout: float = 5) -> bytes:
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"} if body is not None else {}
    with urllib.request.urlopen(
        urllib.request.Request(url, data=data, headers=headers), timeout=timeout
    ) as response:
        return response.read(4096)


def check(env_file: Path):
    values = read_file(env_file)
    llm, model = values["VERIFY_LLM_BASE_URL"], values["VERIFY_LLM_MODEL"]
    problems = []
    try:
        served = [entry.get("id") for entry in json.loads(request(f"{llm}/models")).get("data", [])]
        if model not in served:
            problems.append(
                f"{llm} does not serve {model}"
                f" (it serves {', '.join(map(str, served)) or 'nothing'})."
                f" With Ollama: ollama pull {model}"
            )
        else:
            request(
                f"{llm}/chat/completions",
                {"model": model, "max_tokens": 1, "messages": [{"role": "user", "content": "Hi"}]},
                timeout=120,
            )
    except urllib.error.HTTPError as error:
        detail = error.read(300).decode(errors="replace")
        problems.append(f"{model} at {llm} failed a one-token reply (HTTP {error.code}): {detail}")
    except (urllib.error.URLError, OSError, ValueError) as error:
        problems.append(
            f"no model server answered at {llm}/models ({error}). With Ollama: ollama serve,"
            f" then ollama pull {model}"
        )
    tts, api = values["VERIFY_TTS_BASE_URL"], values["VERIFY_TTS_API"]
    for voice in values["VERIFY_TTS_VOICES"].split(","):
        if api == "openai":
            url = f"{tts}/audio/speech"
            body = {
                "model": values["VERIFY_TTS_MODEL"],
                "voice": voice,
                "input": "Ready.",
                "response_format": "pcm",
                "stream_format": "audio",
            }
        else:
            url, body = f"{tts}/audio/speech/pcm-stream", {"input": "Ready.", "voice": voice}
        try:
            if not request(url, body, timeout=60):
                problems.append(f"{url} returned no audio for voice {voice}")
        except urllib.error.HTTPError as error:
            problems.append(
                f"{url} refused voice {voice} (HTTP {error.code}); set VERIFY_TTS_VOICE and"
                " VERIFY_TTS_VOICES to voices it knows"
            )
        except (urllib.error.URLError, OSError) as error:
            problems.append(
                f"no speech server answered at {url} ({error}). For Kokoro, from the repository:"
                " docker compose --profile kokoro up -d --wait"
            )
            break
    if problems:
        fail("\n  ".join(["not ready:", *problems]))
    print(f"ok    {model} at {llm}; voices {values['VERIFY_TTS_VOICES']} at {tts}")


if __name__ == "__main__":
    if sys.argv[1:2] == ["resolve"] and len(sys.argv) == 3:
        resolve(Path(sys.argv[2]))
    elif sys.argv[1:2] == ["check"] and len(sys.argv) == 3:
        check(Path(sys.argv[2]))
    else:
        sys.exit(__doc__)
