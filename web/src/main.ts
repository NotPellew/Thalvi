import { Connection, sessionUrl } from "./connection.ts";
import { Controller } from "./controller.ts";
import { micErrorMessage, openMic, type Mic } from "./mic.ts";
import { Playback } from "./playback.ts";
import { loadSpeed, saveSpeed } from "./speed.ts";
import { createView } from "./view.ts";
import { holdWakeLock } from "./wake-lock.ts";

const SESSION_KEY = "thalvi.session";

interface VoiceAudio {
  readonly ctx: AudioContext;
  readonly mic: Mic;
  readonly playback: Playback;
  readonly releaseWakeLock: () => void;
}

const appRoot = document.getElementById("app");
if (!appRoot) throw new Error("#app is missing from index.html");
const view = createView(appRoot);
let voiceAudio: VoiceAudio | null = null;
let currentSpeed = loadSpeed(localStorage);
view.setSpeed(currentSpeed);

const controller = new Controller({
  send: (text) => {
    connection.send(text);
  },
  playback: {
    enqueue: (bytes) => {
      voiceAudio?.playback.enqueue(bytes);
    },
    flush: () => {
      voiceAudio?.playback.flush();
    },
  },
  remember: (id) => {
    sessionStorage.setItem(SESSION_KEY, id);
  },
  releaseVoice,
  onChange: (state) => {
    view.render(state);
  },
});

const connection = new Connection(
  () => sessionUrl(location, sessionStorage.getItem(SESSION_KEY)),
  {
    onOpen: () => {
      controller.opened();
    },
    onLost: () => {
      controller.lost();
    },
    onMessage: (data) => {
      controller.message(data);
    },
  },
);

function releaseVoice(): void {
  const current = voiceAudio;
  if (!current) return;
  voiceAudio = null;
  current.mic.stop();
  current.releaseWakeLock();
  void current.ctx.close();
}

function stopVoice(notice: string | null): void {
  if (!voiceAudio) return;
  controller.setVoice(false, notice);
  releaseVoice();
}

async function startVoice(): Promise<void> {
  view.setVoiceBusy(true);
  // Audio must be created and the mic requested inside the tap's user activation.
  const ctx = new AudioContext({ latencyHint: "interactive" });
  try {
    await ctx.resume();
    const mic = await openMic(
      ctx,
      (frame) => {
        if (controller.state.voice) connection.send(frame);
      },
      () => {
        stopVoice("The microphone stopped working.");
      },
    );
    const playback = new Playback(ctx, (playing) => {
      controller.playing(playing);
    });
    playback.setRate(currentSpeed);
    voiceAudio = {
      ctx,
      mic,
      playback,
      releaseWakeLock: holdWakeLock(),
    };
    controller.setVoice(true);
  } catch (error) {
    void ctx.close();
    controller.notify(micErrorMessage(error));
  } finally {
    view.setVoiceBusy(false);
  }
}

view.onSpeed((rate) => {
  currentSpeed = rate;
  saveSpeed(localStorage, rate);
  voiceAudio?.playback.setRate(rate);
});
view.onVoice(() => {
  if (voiceAudio) stopVoice(null);
  else void startVoice();
});
view.onStop(() => {
  controller.stop();
});
view.onSubmit((text) => {
  if (controller.sendText(text)) view.clearComposer();
});
view.render(controller.state);
connection.connect();
