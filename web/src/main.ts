import { Connection, privateSessionUrl, sessionUrl } from "./connection.ts";
import { Controller } from "./controller.ts";
import { conversationsApi } from "./conversations.ts";
import { DeviceAudio } from "./device-audio.ts";
import { Learnings, learningsApi } from "./learnings.ts";
import { createLearningsView } from "./learnings-view.ts";
import { Library } from "./library.ts";
import { createLibraryView } from "./library-view.ts";
import { voiceNameMessage } from "./protocol.ts";
import { loadSpeed, saveSpeed } from "./speed.ts";
import { createView } from "./view.ts";
import { VoicePicker, voiceApi } from "./voice.ts";
import { audioElementPlayer, createVoiceView } from "./voice-view.ts";

const SESSION_KEY = "thalvi.session";
/** The next connection starts a private conversation; a reload never does. */
let wantPrivate = false;

const appRoot = document.getElementById("app");
if (!appRoot) throw new Error("#app is missing from index.html");
const view = createView(appRoot);
const initialSpeed = loadSpeed(localStorage);
view.setSpeed(initialSpeed);

const controller = new Controller({
  send: (text) => {
    connection.send(text);
  },
  playback: {
    enqueue: (bytes) => {
      deviceAudio.playback?.enqueue(bytes);
    },
    flush: () => {
      deviceAudio.playback?.flush();
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
    wantPrivate = false;
    if (id === null) sessionStorage.removeItem(SESSION_KEY);
    else sessionStorage.setItem(SESSION_KEY, id);
    connection.close();
    connection.connect();
  },
  connectPrivate: () => {
    wantPrivate = true;
    connection.close();
    connection.connect();
  },
  releaseUnused: () => {
    deviceAudio.releaseUnused();
  },
  onChange: (state) => {
    view.render(state);
  },
});

const deviceAudio = new DeviceAudio(
  controller,
  {
    setBusy: (busy) => {
      view.setBusy(busy);
    },
    sendFrame: (frame) => {
      connection.send(frame);
    },
  },
  initialSpeed,
);

const connection = new Connection(
  () => (wantPrivate ? privateSessionUrl(location) : sessionUrl(location, sessionStorage.getItem(SESSION_KEY))),
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
  undefined,
  () => !controller.state.private,
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
  startPrivate: () => {
    controller.startPrivate();
  },
});
createLibraryView(appRoot, library, () => sessionStorage.getItem(SESSION_KEY));
createLearningsView(appRoot, new Learnings(learningsApi((input, init) => fetch(input, init))));

view.onSpeed((rate) => {
  saveSpeed(localStorage, rate);
  deviceAudio.setSpeed(rate);
});
view.onVoice(() => {
  if (controller.state.voice) controller.setVoice(false);
  else void deviceAudio.startVoice();
});
view.onSpeak(() => {
  if (controller.state.speak) controller.setSpeak(false);
  else void deviceAudio.startSpeak();
});
view.onDismissNotice(() => {
  controller.dismissNotice();
});
view.onStop(() => {
  controller.stop();
});
view.onSubmit((text) => {
  if (controller.sendText(text)) view.clearComposer();
});
view.render(controller.state);
connection.connect();
