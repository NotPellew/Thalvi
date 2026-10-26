import micWorkletUrl from "./mic-worklet.ts?worker&url";

export interface Mic {
  stop(): void;
}

export function micErrorMessage(error: unknown): string {
  const name = error instanceof DOMException ? error.name : "";
  switch (name) {
    case "NotAllowedError":
    case "SecurityError":
      return "Microphone access was denied. Allow it in your browser settings, then tap Start.";
    case "NotFoundError":
    case "OverconstrainedError":
      return "No microphone was found.";
    default:
      return "The microphone could not be started.";
  }
}

/** Streams 24 kHz s16le frames to `onFrame`; `onEnded` fires if the device goes away. */
export async function openMic(
  ctx: AudioContext,
  onFrame: (frame: ArrayBuffer) => void,
  onEnded: () => void,
): Promise<Mic> {
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
  });
  try {
    await ctx.audioWorklet.addModule(micWorkletUrl);
  } catch (error) {
    stream.getTracks().forEach((track) => {
      track.stop();
    });
    throw error;
  }
  const source = ctx.createMediaStreamSource(stream);
  const node = new AudioWorkletNode(ctx, "mic-encoder");
  node.port.onmessage = (event: MessageEvent<ArrayBuffer>) => {
    onFrame(event.data);
  };
  source.connect(node);
  // Browsers only pull nodes that reach the destination; the worklet outputs silence.
  node.connect(ctx.destination);
  for (const track of stream.getAudioTracks()) track.onended = onEnded;
  return {
    stop() {
      for (const track of stream.getAudioTracks()) track.onended = null;
      source.disconnect();
      node.disconnect();
      node.port.onmessage = null;
      stream.getTracks().forEach((track) => {
        track.stop();
      });
    },
  };
}
