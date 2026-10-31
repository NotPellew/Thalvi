"""Owns the foreground desktop client and its side-effect-free replay command."""

import argparse
import asyncio
import json
import time
from pathlib import Path

from websockets.exceptions import WebSocketException

from .audio import Audio
from .capture import Capture, Confirmation, Ending
from .client import capture, conversation
from .config import Config, record
from .discord import DesktopComposer, PreviewComposer
from .host import desktop
from .metrics import Metrics
from .wake import Wake
from .workflow import PreviewHost, Workflow, notice


async def recover(host, text: str) -> None:
    try:
        await asyncio.to_thread(host.copy, text)
        notice("The transcribed text is on the clipboard.")
    except (RuntimeError, OSError):
        notice(f"Clipboard recovery failed. Your text follows:\n{text}")


async def run(config: Config, once: bool, dry_run: bool) -> int:
    wake = await asyncio.to_thread(Wake, config)
    host = PreviewHost() if dry_run else desktop()
    metrics = Metrics(config.metrics)
    workflow = Workflow(
        config,
        host,
        discord=PreviewComposer() if dry_run else DesktopComposer(config, metrics),
        metrics=metrics,
    )
    current: Capture | None = None
    async with Audio(config.device) as audio:
        notice("Waiting for a wake word. Ctrl+C stops and releases the microphone.")
        try:
            while True:
                mode = await asyncio.to_thread(wake.detect, await audio.next())
                if mode is None:
                    continue
                started = time.monotonic()
                outcome = "failed"
                try:
                    if mode == "conversation":
                        notice("Conversation waking. Use headphones in this native prototype.")
                        await conversation(config, audio, notice)
                    else:
                        selected = await asyncio.to_thread(host.capture)
                        current = Capture(config.endings)
                        notice(f"Capture waking · target: {selected.app}.")
                        request = await capture(config, audio, current, notice)

                        async def confirm() -> bool:
                            try:
                                async with asyncio.timeout(30):
                                    result = await capture(
                                        config,
                                        audio,
                                        Confirmation(),
                                        notice,
                                        ready_message="Say yes to submit, or never mind to cancel.",
                                        fresh_audio=True,
                                    )
                            except TimeoutError as error:
                                raise RuntimeError(
                                    "Confirmation timed out. Nothing was submitted."
                                ) from error
                            return result.ending == Ending.SUBMIT

                        success = await workflow.finish(request, selected, confirm)
                        outcome = "ok" if success else "failed"
                        if not success and once:
                            return 1
                        current = None
                    if mode == "conversation":
                        outcome = "ok"
                except (
                    RuntimeError,
                    OSError,
                    ValueError,
                    TimeoutError,
                    WebSocketException,
                ) as error:
                    notice(f"{error} Nothing will be delivered automatically.")
                    if current is not None and current.text:
                        await recover(host, current.text)
                    current = None
                    if once or audio.failed:
                        return 1
                finally:
                    try:
                        await workflow.metrics.record(
                            "desktop_capture_flow" if mode == "capture" else "desktop_conversation",
                            started,
                            outcome,
                        )
                    except OSError:
                        notice("The optional desktop metrics could not be saved.")
                    wake.reset()
                    audio.discard()
                if once:
                    return 0
                notice("Waiting for a wake word.")
        finally:
            if current is not None and current.text:
                await recover(host, current.text)


async def replay(config: Config, path: Path, apply: bool) -> int:
    host = desktop() if apply else PreviewHost()
    metrics = Metrics(config.metrics)
    workflow = Workflow(
        config,
        host,
        discord=DesktopComposer(config, metrics) if apply else PreviewComposer(),
        metrics=metrics,
    )
    events = iter(path.read_text(encoding="utf-8").splitlines())
    current = None
    selected = None
    successful = True
    for line in events:
        event = record(json.loads(line), "replay event")
        if event.get("wake") == "capture":
            if current is not None:
                raise ValueError("Finish the current replay capture before starting another.")
            current, selected = Capture(config.endings), host.capture()
        elif isinstance(event.get("final"), str) and current is not None:
            if request := current.final(event["final"]):

                async def confirm() -> bool:
                    try:
                        response = record(json.loads(next(events)), "confirmation")
                    except StopIteration as error:
                        raise ValueError(
                            "A submit replay needs an explicit confirmation event."
                        ) from error
                    if type(response.get("confirm")) is not bool:
                        raise ValueError("Confirmation must be true or false.")
                    return response["confirm"]

                successful &= await workflow.finish(request, selected, confirm)
                current, selected = None, None
        else:
            raise ValueError("Replay expects a capture wake event followed by final text events.")
    if current is not None:
        raise ValueError("Replay ended before an ending command. Nothing was delivered.")
    return 0 if successful else 1


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Thalvi's local wake-word and desktop capture client."
    )
    parser.add_argument("--config", type=Path, required=True)
    sub = parser.add_subparsers(dest="command", required=True)
    live = sub.add_parser(
        "run", help="Open the native microphone and listen for configured wake models."
    )
    live.add_argument("--once", action="store_true")
    live.add_argument(
        "--dry-run", action="store_true", help="Transcribe, but preview all desktop operations."
    )
    replay_parser = sub.add_parser(
        "replay", help="Replay JSONL text fixtures without microphone or model calls."
    )
    replay_parser.add_argument("path", type=Path)
    replay_parser.add_argument(
        "--apply", action="store_true", help="Actually deliver to the focused desktop window."
    )
    args = parser.parse_args()
    try:
        config = Config.load(args.config)
        result = asyncio.run(
            run(config, args.once, args.dry_run)
            if args.command == "run"
            else replay(config, args.path, args.apply)
        )
    except KeyboardInterrupt:
        notice("Stopped. Microphone released.")
        result = 0
    except (RuntimeError, OSError, ValueError, TimeoutError, WebSocketException) as error:
        parser.exit(1, f"{error}\n")
    raise SystemExit(result)
