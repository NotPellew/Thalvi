"""Owns the foreground desktop client and its side-effect-free replay command."""

import argparse
import asyncio
import contextlib
import json
import time
from pathlib import Path

from websockets.exceptions import WebSocketException

from .audio import Audio
from .capture import Capture, Confirmation, Ending
from .client import capture, conversation
from .config import Config, record
from .cues import Cue
from .discord import DesktopComposer, PreviewComposer
from .host import desktop
from .metrics import Metrics
from .wake import Wake
from .workflow import PreviewHost, Workflow, notice


def wake_level(frame: bytes) -> str:
    import numpy as np

    peak = int(np.abs(np.frombuffer(frame, dtype="<i2").astype(np.int32)).max(initial=0))
    return "activity" if peak >= 1000 else "quiet"


async def recover(host, text: str) -> None:
    try:
        await asyncio.to_thread(host.copy, text)
        notice("The transcribed text is on the clipboard.")
    except (RuntimeError, OSError):
        notice(f"Clipboard recovery failed. Your text follows:\n{text}")


async def run(config: Config, once: bool, dry_run: bool) -> int:
    result = await _run(config, once, dry_run)
    if once:
        notice("Finished. Microphone released.")
    return result


async def _run(config: Config, once: bool, dry_run: bool, resident=None) -> int:
    metrics = Metrics(config.metrics)
    if resident:
        wake = resident.wake
    else:
        started, outcome = time.monotonic(), "error"
        try:
            wake = await asyncio.to_thread(Wake, config)
            outcome = "ok"
        except asyncio.CancelledError:
            outcome = "cancelled"
            raise
        finally:
            try:
                await metrics.record("desktop_wake_load", started, outcome)
            except OSError:
                notice("The optional desktop metrics could not be saved.")

    inflight_wake = None

    async def finish_wake_work():
        if inflight_wake is not None:
            await asyncio.gather(inflight_wake, return_exceptions=True)

    async def flush_metrics() -> None:
        try:
            await metrics.flush()
        except OSError:
            notice("The optional desktop metrics could not be saved.")

    host = resident.host if resident else (PreviewHost() if dry_run else desktop())
    workflow = Workflow(
        config,
        host,
        discord=PreviewComposer() if dry_run else DesktopComposer(config, metrics),
        metrics=metrics,
        notify=resident.notice if resident else notice,
    )
    current: Capture | None = None
    sampling = metrics.path is not None
    sampled, sampled_level = 0, None
    window_activity = False
    ambiguous = False
    async with contextlib.AsyncExitStack() as resources:
        resources.push_async_callback(flush_metrics)
        resources.push_async_callback(finish_wake_work)
        audio = await resources.enter_async_context(Audio(config.device))
        if resident:
            resident.state("listening", "Listening for a wake word.")
        notice("Waiting for a wake word. Ctrl+C stops and releases the microphone.")
        try:
            while True:
                frame = await audio.next()
                if sampling:
                    sampled += 1
                    level_started = time.monotonic()
                    window_activity |= wake_level(frame) == "activity"
                    if sampled == 1 or sampled % 25 == 0:
                        sampled_level_now = "activity" if window_activity else "quiet"
                        window_activity = False
                        try:
                            await metrics.record(
                                "desktop_wake_level",
                                level_started,
                                sampled_level_now,
                                buffered=True,
                            )
                        except OSError:
                            notice("The optional desktop metrics could not be saved.")
                        if sampled_level_now != sampled_level:
                            sampled_level = sampled_level_now
                            notice(f"Sampled wake audio: {sampled_level}.")
                started, outcome = time.monotonic(), "error"
                try:
                    inflight_wake = asyncio.create_task(asyncio.to_thread(wake.detect, frame))
                    mode = await asyncio.shield(inflight_wake)
                    inflight_wake = None
                    outcome = mode or "none"
                except asyncio.CancelledError:
                    outcome = "cancelled"
                    raise
                finally:
                    try:
                        await metrics.record(
                            "desktop_wake_inference", started, outcome, buffered=True
                        )
                    except OSError:
                        notice("The optional desktop metrics could not be saved.")
                if mode == "ambiguous":
                    if not ambiguous:
                        notice("Wake was ambiguous; still listening.")
                    ambiguous = True
                    continue
                ambiguous = False
                if mode is None:
                    continue
                if resident:
                    resident.begin()
                    host = resident.host
                    workflow.host = host
                started = time.monotonic()
                outcome = "failed"
                try:
                    if mode == "conversation":
                        notice("Conversation waking. Use headphones in this native prototype.")
                        await conversation(config, audio, notice)
                    else:
                        if not resident:
                            notice("Capture waking. Reading the target window.")
                        audio.set_input(False)
                        target_started, target_outcome = time.monotonic(), "error"
                        try:
                            selected = await asyncio.to_thread(host.capture)
                            target_outcome = "ok"
                        except asyncio.CancelledError:
                            target_outcome = "cancelled"
                            raise
                        finally:
                            try:
                                await metrics.record(
                                    "desktop_target_capture",
                                    target_started,
                                    target_outcome,
                                    buffered=True,
                                )
                            except OSError:
                                notice("The optional desktop metrics could not be saved.")
                        if resident:
                            resident.state("capturing", "Capturing speech.", selected.app)
                        current = Capture(config.endings)
                        notice(f"Capture waking · target: {selected.app}.")

                        def pending_notice(request, seconds, target_app=selected.app):
                            if request is None:
                                if resident:
                                    resident.state("capturing", "Speech continued.", target_app)
                            elif resident:
                                resident.pending(request, seconds, target_app)
                            else:
                                notice(
                                    f"Confirm {request.ending} to {target_app}: pause {seconds:g} "
                                    "seconds to continue; keep speaking or cancel."
                                )

                        request = await capture(
                            config,
                            audio,
                            current,
                            notice,
                            metrics=metrics,
                            pending_notice=pending_notice,
                        )

                        async def confirm(target_app=selected.app) -> bool:
                            if resident:
                                resident.state(
                                    "confirming", "Waiting for confirmation.", target_app
                                )
                            try:
                                async with asyncio.timeout(30):
                                    result = await capture(
                                        config,
                                        audio,
                                        Confirmation(),
                                        notice,
                                        ready_message="Say yes to submit, or never mind to cancel.",
                                        metrics=metrics,
                                        confirmation=True,
                                    )
                            except TimeoutError as error:
                                raise RuntimeError(
                                    "Confirmation timed out. Nothing was submitted."
                                ) from error
                            return result.ending == Ending.SUBMIT

                        if resident:
                            resident.state("working", "Processing your request.", selected.app)
                        success = await workflow.finish(request, selected, confirm)
                        outcome = "ok" if success else "failed"
                        if not success and config.cues.enabled(Cue.ERROR):
                            with contextlib.suppress(RuntimeError):
                                await audio.play_cue(Cue.ERROR)
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
                    if config.cues.enabled(Cue.ERROR):
                        with contextlib.suppress(RuntimeError):
                            await audio.play_cue(Cue.ERROR)
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
                if once:
                    return 0
                audio.set_input(False)
                reset_started, reset_outcome = time.monotonic(), "error"
                try:
                    inflight_wake = asyncio.create_task(asyncio.to_thread(wake.reset))
                    await asyncio.shield(inflight_wake)
                    inflight_wake = None
                    reset_outcome = "ok"
                except asyncio.CancelledError:
                    reset_outcome = "cancelled"
                    raise
                except Exception:
                    notice(
                        "Wake listening could not restart; microphone closing. "
                        "The action outcome is unchanged."
                    )
                    return 1
                finally:
                    try:
                        await metrics.record(
                            "desktop_wake_reset", reset_started, reset_outcome, buffered=True
                        )
                    except OSError:
                        notice("The optional desktop metrics could not be saved.")
                audio.set_input(True)
                if resident:
                    resident.end()
                    resident.state("listening", "Listening for a wake word.")
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
    sub.add_parser("app", help="Open the resident desktop app.")
    replay_parser = sub.add_parser(
        "replay", help="Replay JSONL text fixtures without microphone or model calls."
    )
    replay_parser.add_argument("path", type=Path)
    replay_parser.add_argument(
        "--apply", action="store_true", help="Actually deliver to the focused desktop window."
    )
    args = parser.parse_args()
    try:
        if args.command == "app":
            from .app import launch

            raise SystemExit(launch(args.config.resolve()))
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
