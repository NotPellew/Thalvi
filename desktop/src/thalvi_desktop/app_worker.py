"""Owns the resident desktop worker and its line-oriented UI protocol."""

import argparse
import asyncio
import contextlib
import json
import sys
import threading
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from time import monotonic

from . import cli
from .config import Config
from .host import CommandRunner, Hyprland, desktop
from .metrics import Metrics
from .wake import Wake


class Resident:
    def __init__(self, wake, host):
        self.wake, self.host = wake, host
        self.runner = None
        self.capture_id = 0
        self.active = False

    def emit(self, event):
        print(json.dumps(event, ensure_ascii=False), flush=True)

    def notice(self, message):
        self.emit({"type": "notice", "message": str(message)[:100_000]})

    def state(self, phase, message, target=None):
        event = {"type": "state", "phase": phase, "message": message}
        if self.active:
            event["capture_id"] = self.capture_id
        if target:
            event["target"] = target
        self.emit(event)

    def pending(self, request, seconds, target):
        action = "send" if request.ending == "submit" else "continue"
        message = (
            f"Confirm {request.ending} to {target}: pause {seconds:g} seconds to {action}; "
            "keep speaking or cancel to continue."
        )
        self.emit(
            {
                "type": "state",
                "phase": "pending",
                "message": message,
                "target": target,
                "ending": request.ending,
                "capture_id": self.capture_id,
                "remaining_seconds": seconds,
            }
        )
        if request.ending == "submit":
            self.notice(request.text)

    def begin(self):
        self.capture_id += 1
        self.active = True
        self.runner = CommandRunner()
        self.host = Hyprland(self.runner)

    def end(self):
        self.active = False
        self.runner = None

    def cancel(self):
        if self.runner:
            self.runner.cancel()


class CommandKind(StrEnum):
    LISTEN = "listen"
    PAUSE = "pause"
    CANCEL = "cancel"
    QUIT = "quit"


@dataclass(frozen=True)
class Command:
    kind: CommandKind
    capture_id: int | None = None


def parse_command(line: bytes) -> Command:
    if len(line) > 4096 or not line.endswith(b"\n"):
        raise ValueError("Invalid command.")
    try:
        value = json.loads(line)
        if not isinstance(value, dict) or set(value) - {"command", "capture_id"}:
            raise ValueError
        kind = CommandKind(value.get("command"))
        capture_id = value.get("capture_id")
        if capture_id is not None and (type(capture_id) is not int or capture_id < 1):
            raise ValueError
        if kind != CommandKind.CANCEL and capture_id is not None:
            raise ValueError
        return Command(kind, capture_id)
    except (ValueError, TypeError, UnicodeDecodeError) as error:
        raise ValueError("Invalid command.") from error


class CommandInput:
    def __init__(self, size=32):
        self.queue = asyncio.Queue(maxsize=size)
        self.failed = False

    def put(self, command):
        if self.failed:
            return
        try:
            self.queue.put_nowait(command)
        except asyncio.QueueFull:
            self.failed = True
            while not self.queue.empty():
                self.queue.get_nowait()
            self.queue.put_nowait(None)

    async def get(self):
        return await self.queue.get()


def start_stdin(commands):
    loop = asyncio.get_running_loop()

    def deliver(command):
        with contextlib.suppress(RuntimeError):
            loop.call_soon_threadsafe(commands.put, command)

    def read():
        while line := sys.stdin.buffer.readline(4097):
            try:
                deliver(parse_command(line))
            except ValueError:
                deliver(None)
                return
        deliver(Command(CommandKind.QUIT))

    threading.Thread(target=read, daemon=True).start()


async def serve(config, commands=None):
    def emit(phase, message):
        print(json.dumps({"type": "state", "phase": phase, "message": message}), flush=True)

    emit("loading", "Loading the wake model.")
    metrics = Metrics(config.metrics)
    started, outcome = monotonic(), "error"
    try:
        wake = await asyncio.to_thread(Wake, config)
        outcome = "ok"
    except asyncio.CancelledError:
        outcome = "cancelled"
        raise
    except (RuntimeError, OSError, ValueError) as error:
        emit("error", str(error))
        return
    finally:
        try:
            await metrics.record("desktop_wake_load", started, outcome)
        except OSError:
            emit("error", "The optional desktop metrics could not be saved.")
    try:
        host = desktop()
    except (RuntimeError, OSError, ValueError) as error:
        emit("error", str(error))
        return
    resident = Resident(wake, host)
    old_notice = cli.notice
    cli.notice = resident.notice
    resident.state("paused", "Ready. Enable listening to open the microphone.")
    if commands is None:
        commands = CommandInput()
        start_stdin(commands)
    task = None
    listening = False
    try:
        while True:
            command_task = asyncio.create_task(commands.get())
            waiting = {command_task} | ({task} if task else set())
            done, _ = await asyncio.wait(waiting, return_when=asyncio.FIRST_COMPLETED)
            if task and task in done:
                failure = None
                try:
                    await task
                except asyncio.CancelledError:
                    pass
                except Exception as error:
                    failure = str(error)
                    resident.state("error", failure)
                task = None
                listening = False
                resident.end()
                message = (
                    f"{failure} Enable listening to try again."
                    if failure
                    else "Listening stopped. Enable it to try again."
                )
                resident.state("paused", message)
            if command_task not in done:
                command_task.cancel()
                await asyncio.gather(command_task, return_exceptions=True)
                continue
            command = command_task.result()
            if command is None:
                resident.state(
                    "error", "Invalid command or command queue overflow. Thalvi stopped."
                )
                break
            if command.kind == CommandKind.LISTEN and not listening:
                try:
                    await asyncio.to_thread(wake.reset)
                except Exception as error:
                    resident.state("error", str(error))
                    resident.state("paused", f"{error} Enable listening to try again.")
                    continue
                listening = True
                task = asyncio.create_task(cli._run(config, False, False, resident))
            elif command.kind in (CommandKind.PAUSE, CommandKind.QUIT) or (
                command.kind == CommandKind.CANCEL
                and resident.active
                and command.capture_id == resident.capture_id
            ):
                resident.cancel()
                if task:
                    task.cancel()
                    await asyncio.gather(task, return_exceptions=True)
                    task = None
                resident.end()
                if command.kind == CommandKind.QUIT:
                    break
                if command.kind == CommandKind.CANCEL and listening:
                    try:
                        await asyncio.to_thread(wake.reset)
                    except Exception as error:
                        resident.state("error", str(error))
                        listening = False
                        resident.state("paused", "Listening stopped. Enable it to try again.")
                        continue
                    task = asyncio.create_task(cli._run(config, False, False, resident))
                else:
                    listening = False
                    resident.state("paused", "Microphone paused.")
    finally:
        resident.cancel()
        if task:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
        cli.notice = old_notice
        resident.state("stopped", "Thalvi stopped.")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, required=True)
    args = parser.parse_args()
    try:
        asyncio.run(serve(Config.load(args.config)))
    except (RuntimeError, OSError, ValueError) as error:
        print(json.dumps({"type": "state", "phase": "error", "message": str(error)}), flush=True)
        print(
            json.dumps({"type": "state", "phase": "stopped", "message": "Thalvi stopped."}),
            flush=True,
        )


if __name__ == "__main__":
    main()
