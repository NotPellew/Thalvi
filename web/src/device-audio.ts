import type { Controller } from "./controller.ts";
import { micErrorMessage, openMic, type Mic } from "./mic.ts";
import { Playback } from "./playback.ts";
import type { Speed } from "./speed.ts";
import { holdWakeLock } from "./wake-lock.ts";

interface ReplyAudio {
  readonly ctx: AudioContext;
  readonly playback: Playback;
}

interface MicInput {
  readonly mic: Mic;
  readonly releaseWakeLock: () => void;
}

export interface DeviceAudioPorts {
  setBusy(busy: "mic" | "audio" | null): void;
  sendFrame(frame: ArrayBuffer): void;
}

/** Owns the browser's audio context and microphone, and starts and releases them for the controller. */
export class DeviceAudio {
  private readonly controller: Controller;
  private readonly ports: DeviceAudioPorts;
  private speed: Speed;
  private audio: ReplyAudio | null = null;
  private micInput: MicInput | null = null;

  constructor(controller: Controller, ports: DeviceAudioPorts, speed: Speed) {
    this.controller = controller;
    this.ports = ports;
    this.speed = speed;
  }

  get playback(): Playback | null {
    return this.audio?.playback ?? null;
  }

  setSpeed(rate: Speed): void {
    this.speed = rate;
    this.audio?.playback.setRate(rate);
  }

  releaseUnused(): void {
    const { voice, speak } = this.controller.state;
    if (!voice && this.micInput) {
      this.micInput.mic.stop();
      this.micInput.releaseWakeLock();
      this.micInput = null;
    }
    if (!voice && !speak && this.audio) {
      void this.audio.ctx.close();
      this.audio = null;
    }
  }

  /** Must be called inside the tap's user activation, because it creates the audio context. */
  private async acquire(): Promise<ReplyAudio> {
    if (this.audio) return this.audio;
    const ctx = new AudioContext({ latencyHint: "interactive" });
    try {
      await ctx.resume();
    } catch (error) {
      void ctx.close();
      throw error;
    }
    const playback = new Playback(ctx, (playing) => {
      this.controller.playing(playing);
    });
    playback.setRate(this.speed);
    this.audio = { ctx, playback };
    return this.audio;
  }

  async startVoice(): Promise<void> {
    this.ports.setBusy("mic");
    try {
      const { ctx } = await this.acquire();
      const mic = await openMic(
        ctx,
        (frame) => {
          if (this.controller.state.voice) this.ports.sendFrame(frame);
        },
        () => {
          if (this.micInput) this.controller.micStopped("The microphone stopped working.");
        },
      );
      if (this.controller.state.link === "ended") {
        mic.stop();
        this.releaseUnused();
        return;
      }
      this.micInput = { mic, releaseWakeLock: holdWakeLock() };
      this.controller.setVoice(true);
    } catch (error) {
      this.releaseUnused();
      if (this.controller.state.link !== "ended") this.controller.notify(micErrorMessage(error));
    } finally {
      this.ports.setBusy(null);
    }
  }

  async startSpeak(): Promise<void> {
    this.ports.setBusy("audio");
    try {
      await this.acquire();
      this.controller.setSpeak(true);
    } catch {
      this.releaseUnused();
      this.controller.notify("Couldn't start audio on this device.");
    } finally {
      this.ports.setBusy(null);
    }
  }
}
