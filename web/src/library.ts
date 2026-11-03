import type { ConversationDetail, ConversationsApi, ConversationSummary } from "./conversations.ts";

export interface LibraryState {
  readonly open: boolean;
  readonly loading: boolean;
  readonly items: readonly ConversationSummary[];
  /** The conversation being read; joining it is a separate, explicit step. */
  readonly preview: ConversationDetail | null;
  /** The conversation whose title is being edited. */
  readonly renaming: string | null;
  /** The conversation waiting for the user to confirm its deletion. */
  readonly confirmingDelete: string | null;
  readonly error: string | null;
}

export interface LibraryPorts {
  api: ConversationsApi;
  /** The conversation this device has open, if the server has told it one. */
  currentId(): string | null;
  /** Joins `id`, taking it over from any other device. */
  open(id: string): void;
  startNew(): void;
}

/** The control the view should focus after the list changes from `previous` to `state`. */
export type FocusTarget =
  | { readonly control: "new" | "opener" | "rename-input" | "keep" | "continue" }
  | { readonly control: "rename" | "delete" | "row"; readonly id: string };

export function focusTarget(previous: LibraryState, state: LibraryState): FocusTarget | null {
  if (state.open && !previous.open) return { control: "new" };
  if (!state.open && previous.open) return { control: "opener" };
  if (!state.open) return null;
  if (state.preview !== null && previous.preview === null) return { control: "continue" };
  if (state.preview === null && previous.preview !== null) return { control: "row", id: previous.preview.id };
  if (state.renaming !== null && state.renaming !== previous.renaming) return { control: "rename-input" };
  if (state.confirmingDelete !== null && state.confirmingDelete !== previous.confirmingDelete) return { control: "keep" };
  if (previous.renaming !== null && state.renaming === null) return { control: "rename", id: previous.renaming };
  const deleted = previous.confirmingDelete;
  if (deleted !== null && state.confirmingDelete === null) {
    if (state.items.some((item) => item.id === deleted)) return { control: "delete", id: deleted };
    const next = state.items[previous.items.findIndex((item) => item.id === deleted)];
    return next ? { control: "row", id: next.id } : { control: "new" };
  }
  return null;
}

const CLOSED: LibraryState = {
  open: false,
  loading: false,
  items: [],
  preview: null,
  renaming: null,
  confirmingDelete: null,
  error: null,
};

function plain(error: unknown): string {
  return error instanceof Error ? error.message : "Something went wrong.";
}

const CHANGED = "Another device started using this conversation while you were reading it. Check it, then choose Continue here again.";

/** The conversation list: opened only by the user. Reading never joins; deletion is always confirmed first. */
export class Library {
  state: LibraryState = CLOSED;
  private readonly ports: LibraryPorts;
  private handler: (state: LibraryState) => void = () => undefined;
  private reading: string | null = null;
  private continuing = false;

  constructor(ports: LibraryPorts) {
    this.ports = ports;
  }

  onChange(handler: (state: LibraryState) => void): void {
    this.handler = handler;
  }

  async show(): Promise<void> {
    this.set({ ...CLOSED, open: true, loading: true });
    await this.reload();
  }

  hide(): void {
    this.reading = null;
    this.set(CLOSED);
  }

  /** Reads `id` for the user to look at; nothing on the server changes hands. */
  async read(id: string): Promise<void> {
    this.reading = id;
    this.set({ ...this.state, loading: true, renaming: null, confirmingDelete: null, error: null });
    try {
      const preview = await this.ports.api.detail(id);
      if (this.reading !== id) return;
      this.set({ ...this.state, loading: false, preview });
    } catch (failure) {
      if (this.reading !== id) return;
      await this.reload(plain(failure));
    }
  }

  closePreview(): void {
    this.reading = null;
    this.set({ ...this.state, preview: null, error: null });
  }

  /**
   * Joins the conversation being read, after checking it still exists. One that became
   * active on another device since it was read needs a second, informed choice.
   */
  async continueHere(): Promise<void> {
    const shown = this.state.preview;
    if (shown === null || this.continuing) return;
    if (shown.id === this.ports.currentId()) {
      this.hide();
      return;
    }
    let latest: ConversationDetail;
    this.continuing = true;
    try {
      latest = await this.ports.api.detail(shown.id);
    } catch (failure) {
      if (this.reading !== shown.id) return;
      this.reading = null;
      await this.reload(plain(failure), { preview: null });
      return;
    } finally {
      this.continuing = false;
    }
    // Leaving the preview while the check ran (back, close, New conversation) cancels joining.
    if (this.reading !== shown.id) return;
    if (latest.live && !shown.live) {
      this.set({ ...this.state, preview: latest, error: CHANGED });
      return;
    }
    this.ports.open(shown.id);
    this.hide();
  }

  startNew(): void {
    this.ports.startNew();
    this.hide();
  }

  askDelete(id: string): void {
    this.set({ ...this.state, confirmingDelete: id, renaming: null, error: null });
  }

  cancelDelete(): void {
    this.set({ ...this.state, confirmingDelete: null });
  }

  async confirmDelete(): Promise<void> {
    const id = this.state.confirmingDelete;
    if (id === null) return;
    let error: string | null = null;
    try {
      await this.ports.api.remove(id);
      if (id === this.ports.currentId()) this.ports.startNew();
    } catch (failure) {
      error = plain(failure);
    }
    await this.reload(error, { confirmingDelete: null });
  }

  startRename(id: string): void {
    this.set({ ...this.state, renaming: id, confirmingDelete: null, error: null });
  }

  cancelRename(): void {
    this.set({ ...this.state, renaming: null });
  }

  async rename(id: string, title: string): Promise<void> {
    const trimmed = title.trim();
    if (trimmed === "") {
      this.cancelRename();
      return;
    }
    let error: string | null = null;
    try {
      await this.ports.api.rename(id, trimmed);
    } catch (failure) {
      error = plain(failure);
    }
    await this.reload(error, { renaming: null });
  }

  /** Refreshes the list; `settled` is applied in the same update, so the view renders once with the final rows. */
  private async reload(error: string | null = null, settled: Partial<LibraryState> = {}): Promise<void> {
    try {
      const items = await this.ports.api.list();
      this.set({ ...this.state, ...settled, loading: false, items, error });
    } catch (failure) {
      this.set({ ...this.state, ...settled, loading: false, error: error ?? plain(failure) });
    }
  }

  private set(state: LibraryState): void {
    this.state = state;
    this.handler(state);
  }
}
