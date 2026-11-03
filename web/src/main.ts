import { Connection, sessionUrl } from "./connection.ts";
import { Controller } from "./controller.ts";
import { conversationsApi } from "./conversations.ts";
import { Learnings, learningsApi } from "./learnings.ts";
import { createLearningsView } from "./learnings-view.ts";
import { Library } from "./library.ts";
import { createLibraryView } from "./library-view.ts";
import { micErrorMessage, openMic, type Mic } from "./mic.ts";
import { Playback } from "./playback.ts";
import { voiceNameMessage } from "./protocol.ts";
import { loadSpeed, saveSpeed, type Speed } from "./speed.ts";
import { createView } from "./view.ts";
import { VoicePicker, voiceApi } from "./voice.ts";
import { audioElementPlayer, createVoiceView } from "./voice-view.ts";
import { holdWakeLock } from "./wake-lock.ts";

const SESSION_KEY = "thalvi.session";

interface ReplyAudio {
  readonly ctx: AudioContext;
  readonly playback: Playback;
}

interface MicInput {
  readonly mic: Mic;
  readonly releaseWakeLock: () => void;
}

const appRoot = document.getElementById("app");
if (!appRoot) throw new Error("#app is missing from index.html");
const view = createView(appRoot);
let audio: ReplyAudio | null = null;
let micInput: MicInput | null = null;
let currentSpeed: Speed = loadSpeed(localStorage);
view.setSpeed(currentSpeed);

const controller = new Controller({
  send: (text) => {
    connection.send(text);
  },
  playback: {
    enqueue: (bytes) => {
      audio?.playback.enqueue(bytes);
    },
    flush: () => {
      audio?.playback.flush();
    },
  },
  remember: (id) => {
    sessionStorage.setItem(SESSION_KEY, id);
  },
  forget: () => {
    sessionStorage.removeItem(SESSION_KEY);
  },
  stopConnection: () => {
    connection.close();
  },
  connect: (id) => {
    if (id === null) sessionStorage.removeItem(SESSION_KEY);
    else sessionStorage.setItem(SESSION_KEY, id);
    connection.close();
    connection.connect();
  },
  releaseUnused,
  onChange: (state) => {
    view.render(state);
  },
});

const connection = new Connection(
  () => sessionUrl(location, sessionStorage.getItem(SESSION_KEY)),
  {
    onOpen: () => {
      controller.opened();
      void voicePicker.socketOpened();
    },
    onLost: () => {
      controller.lost();
      voicePicker.socketLost();
    },
    onMessage: (data) => {
      controller.message(data);
    },
  },
);

const voicePicker = new VoicePicker({
  api: voiceApi((input, init) => fetch(input, init)),
  player: audioElementPlayer(),
  storage: localStorage,
  send: (voice) => {
    connection.send(voiceNameMessage(voice));
  },
});
createVoiceView(appRoot, voicePicker);
void voicePicker.load();

const library = new Library({
  api: conversationsApi((input, init) => fetch(input, init)),
  currentId: () => sessionStorage.getItem(SESSION_KEY),
  open: (id) => {
    controller.switchTo(id);
  },
  startNew: () => {
    controller.switchTo(null);
  },
});
createLibraryView(appRoot, library, () => sessionStorage.getItem(SESSION_KEY));
createLearningsView(appRoot, new Learnings(learningsApi((input, init) => fetch(input, init))));

function releaseUnused(): void {
  const { voice, speak } = controller.state;
  if (!voice && micInput) {
    micInput.mic.stop();
    micInput.releaseWakeLock();
    micInput = null;
  }
  if (!voice && !speak && audio) {
    void audio.ctx.close();
    audio = null;
  }
}

/** Must be called inside the tap's user activation, because it creates the audio context. */
async function acquireAudio(): Promise<ReplyAudio> {
  if (audio) return audio;
  const ctx = new AudioContext({ latencyHint: "interactive" });
  try {
    await ctx.resume();
  } catch (error) {
    void ctx.close();
    throw error;
  }
  const playback = new Playback(ctx, (playing) => {
    controller.playing(playing);
  });
  playback.setRate(currentSpeed);
  audio = { ctx, playback };
  return audio;
}

async function startVoice(): Promise<void> {
  view.setBusy("mic");
  try {
    const { ctx } = await acquireAudio();
    const mic = await openMic(
      ctx,
      (frame) => {
        if (controller.state.voice) connection.send(frame);
      },
      () => {
        if (micInput) controller.micStopped("The microphone stopped working.");
      },
    );
    micInput = { mic, releaseWakeLock: holdWakeLock() };
    controller.setVoice(true);
  } catch (error) {
    releaseUnused();
    controller.notify(micErrorMessage(error));
  } finally {
    view.setBusy(null);
  }
}

async function startSpeak(): Promise<void> {
  view.setBusy("audio");
  try {
    await acquireAudio();
    controller.setSpeak(true);
  } catch {
    releaseUnused();
    controller.notify("Couldn't start audio on this device.");
  } finally {
    view.setBusy(null);
  }
}

view.onSpeed((rate) => {
  currentSpeed = rate;
  saveSpeed(localStorage, rate);
  audio?.playback.setRate(rate);
});
view.onVoice(() => {
  if (controller.state.voice) controller.setVoice(false);
  else void startVoice();
});
view.onSpeak(() => {
  if (controller.state.speak) controller.setSpeak(false);
  else void startSpeak();
});
view.onStop(() => {
  controller.stop();
});
view.onSubmit((text) => {
  if (controller.sendText(text)) view.clearComposer();
});
view.render(controller.state);
connection.connect();
