import type { ConversationsApi, ConversationSummary } from "./conversations.ts";

export interface LibraryState {
  readonly open: boolean;
  readonly loading: boolean;
  readonly items: readonly ConversationSummary[];
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
  open(id: string): void;
  startNew(): void;
}

const CLOSED: LibraryState = {
  open: false,
  loading: false,
  items: [],
  renaming: null,
  confirmingDelete: null,
  error: null,
};

function plain(error: unknown): string {
  return error instanceof Error ? error.message : "Something went wrong.";
}

/** The conversation list: opened only by the user, with deletion always confirmed first. */
export class Library {
  state: LibraryState = CLOSED;
  private readonly ports: LibraryPorts;
  private handler: (state: LibraryState) => void = () => undefined;

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
    this.set(CLOSED);
  }

  /** Joins `id`; the one already open here just closes the list. */
  reopen(id: string): void {
    if (id !== this.ports.currentId()) this.ports.open(id);
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
    this.set({ ...this.state, confirmingDelete: null });
    let error: string | null = null;
    try {
      await this.ports.api.remove(id);
      if (id === this.ports.currentId()) this.ports.startNew();
    } catch (failure) {
      error = plain(failure);
    }
    await this.reload(error);
  }

  startRename(id: string): void {
    this.set({ ...this.state, renaming: id, confirmingDelete: null, error: null });
  }

  cancelRename(): void {
    this.set({ ...this.state, renaming: null });
  }

  async rename(id: string, title: string): Promise<void> {
    const trimmed = title.trim();
    if (trimmed === "") return;
    this.set({ ...this.state, renaming: null });
    let error: string | null = null;
    try {
      await this.ports.api.rename(id, trimmed);
    } catch (failure) {
      error = plain(failure);
    }
    await this.reload(error);
  }

  private async reload(error: string | null = null): Promise<void> {
    try {
      const items = await this.ports.api.list();
      this.set({ ...this.state, loading: false, items, error });
    } catch (failure) {
      this.set({ ...this.state, loading: false, error: error ?? plain(failure) });
    }
  }

  private set(state: LibraryState): void {
    this.state = state;
    this.handler(state);
  }
}
