import { Connection, sessionUrl } from "./connection.ts";
import { Controller } from "./controller.ts";
import { micErrorMessage, openMic, type Mic } from "./mic.ts";
import { Playback } from "./playback.ts";
import { createView } from "./view.ts";
import { holdWakeLock } from "./wake-lock.ts";

interface Running {
  readonly ctx: AudioContext;
  readonly mic: Mic;
  readonly connection: Connection;
  readonly releaseWakeLock: () => void;
}

const view = createView();
let running: Running | null = null;
let playback: Playback | null = null;

const controller = new Controller({
  send: (text) => {
    running?.connection.send(text);
  },
  playback: {
    enqueue: (bytes) => {
      playback?.enqueue(bytes);
    },
    flush: () => {
      playback?.flush();
    },
  },
  onChange: (state) => {
    view.render(state);
  },
});

function teardown(): void {
  const current = running;
  if (!current) return;
  running = null;
  current.connection.close();
  current.mic.stop();
  current.releaseWakeLock();
  void current.ctx.close();
  playback = null;
}

function endSession(): void {
  teardown();
  controller.end();
}

async function startSession(): Promise<void> {
  view.setStarting(true);
  // Audio must be created and the mic requested inside the tap's user activation.
  const ctx = new AudioContext({ latencyHint: "interactive" });
  try {
    await ctx.resume();
    const connection = new Connection(sessionUrl(location), {
      onOpen: () => {
        controller.opened();
      },
      onLost: () => {
        controller.lost();
      },
      onMessage: (data) => {
        controller.message(data);
      },
    });
    const mic = await openMic(
      ctx,
      (frame) => {
        connection.send(frame);
      },
      () => {
        teardown();
        controller.failed("The microphone stopped working.");
      },
    );
    playback = new Playback(ctx, (playing) => {
      controller.playing(playing);
    });
    running = { ctx, mic, connection, releaseWakeLock: holdWakeLock() };
    controller.begin();
    connection.connect();
  } catch (error) {
    void ctx.close();
    controller.failed(micErrorMessage(error));
  } finally {
    view.setStarting(false);
  }
}

view.onStart(() => {
  void startSession();
});
view.onStop(() => {
  controller.stop();
});
view.onEnd(endSession);
view.render(controller.state);
