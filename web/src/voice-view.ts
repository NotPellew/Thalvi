import type { PreviewPlayer, VoicePicker, VoicePickerState } from "./voice.ts";

const CHIP = '<button class="o-voice-chip" type="button" aria-expanded="false" aria-controls="o-vp">Voice</button>';

const PANEL = `
  <section class="o-vp" id="o-vp" aria-label="Voice" hidden>
    <p class="o-vp-note" role="status"></p>
    <div class="o-vp-row">
      <select class="o-vp-select" aria-label="Voice to try"></select>
      <button class="o-vp-act o-vp-preview" type="button">Preview</button>
      <button class="o-vp-act o-vp-apply" type="button">Apply</button>
    </div>
  </section>`;

function find<T extends Element>(root: ParentNode, selector: string, type: new () => T): T {
  const found = root.querySelector(selector);
  if (!(found instanceof type)) throw new Error(`${selector} is missing from the voice markup`);
  return found;
}

/** Plays a preview through an audio element; only ever called from the user's Preview tap. */
export function audioElementPlayer(): PreviewPlayer {
  let current: { audio: HTMLAudioElement; url: string; finish: (error?: Error) => void } | null = null;

  function stop(): void {
    current?.finish();
  }

  return {
    stop,
    play(wav) {
      stop();
      return new Promise<void>((resolve, reject) => {
        const url = URL.createObjectURL(new Blob([wav], { type: "audio/wav" }));
        const audio = new Audio(url);
        const entry = {
          audio,
          url,
          finish: (error?: Error) => {
            if (current !== entry) return;
            current = null;
            audio.onended = null;
            audio.onerror = null;
            audio.pause();
            URL.revokeObjectURL(url);
            if (error) reject(error);
            else resolve();
          },
        };
        current = entry;
        audio.onended = () => {
          entry.finish();
        };
        audio.onerror = () => {
          entry.finish(new Error("Couldn't play the preview on this device."));
        };
        audio.play().catch(() => {
          entry.finish(new Error("Couldn't play the preview on this device."));
        });
      });
    },
  };
}

const PREVIEW_LABEL = { idle: "Preview", loading: "Loading…", playing: "Stop" } as const;

/** Adds the collapsed Voice chip next to the speed chip and its panel below the speed row. */
export function createVoiceView(root: ParentNode, picker: VoicePicker): void {
  const actions = find(root, ".o-head-actions", HTMLDivElement);
  find(root, ".o-speed-row", HTMLDivElement).insertAdjacentHTML("afterend", PANEL);
  actions.insertAdjacentHTML("afterbegin", CHIP);
  const chip = find(actions, ".o-voice-chip", HTMLButtonElement);
  const panel = find(root, ".o-vp", HTMLElement);
  const note = find(panel, ".o-vp-note", HTMLParagraphElement);
  const select = find(panel, ".o-vp-select", HTMLSelectElement);
  const preview = find(panel, ".o-vp-preview", HTMLButtonElement);
  const apply = find(panel, ".o-vp-apply", HTMLButtonElement);

  function render(state: VoicePickerState): void {
    chip.setAttribute("aria-expanded", String(state.open));
    panel.hidden = !state.open;
    note.textContent = state.error ?? state.notice ?? (state.loading ? "Loading voices…" : "");
    select.replaceChildren(
      ...state.voices.map((voice) => {
        const option = document.createElement("option");
        option.value = voice;
        option.textContent = voice === state.applied ? `${voice} (in use)` : voice;
        option.selected = voice === state.candidate;
        return option;
      }),
    );
    select.disabled = state.candidate === null;
    preview.textContent = PREVIEW_LABEL[state.preview];
    preview.disabled = state.candidate === null || state.preview === "loading";
    apply.disabled = state.candidate === null || state.candidate === state.applied;
  }

  picker.onChange(render);
  render(picker.state);

  chip.addEventListener("click", () => {
    if (picker.state.open) picker.hide();
    else picker.show();
  });
  select.addEventListener("change", () => {
    picker.choose(select.value);
  });
  preview.addEventListener("click", () => {
    void picker.preview();
  });
  apply.addEventListener("click", () => {
    picker.apply();
  });
  panel.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    picker.hide();
    chip.focus();
  });
}
