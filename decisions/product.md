# Product and platforms

What Thalvi is, the desktops it runs on and what it is built from.

- **Thalvi is the desktop voice accessory.** The Rust desktop app owns native audio, wake words,
  recognition and window delivery, and remembers nothing beyond its settings, opt-in recordings and
  dictation history. Why: Vardli owns anything that remembers, decides or acts while the user is away,
  and the conversation server duplicated it. Rejected: Thalvi as Vardli's voice front end, freezing the
  server (it lives at tag `conversation-final`).
- **Desktop on Hyprland, GNOME Wayland and Windows.** GNOME gets dictation on
  the clipboard only: it exposes neither the focused window nor focus by ID.
  Why: the desktops we can qualify. Rejected: KDE, X11 and macOS until needed,
  and claiming untested platform parity.
- **Rust desktop app; Tauri settings, CPU-drawn overlay.** One process runs engine, tray, a Tauri settings window and an overlay drawn on its own thread with tiny-skia onto each desktop's surface: a layer surface on Hyprland, an override-redirect XWayland window on GNOME (GTK's X11 backend), a layered non-activating window on Windows; it never takes focus.
  Why: no IPC; the overlay holds no WebView or GPU device (Windows GPU memory 27–33 → 0 MiB, binary +4 MB, #384); on GNOME's Mutter a Wayland window and `wl-copy` each take focus, an X11 window and clipboard do not (measured).
  Rejected: WebView overlay (per-desktop focus tricks; a layer-shell WebView is blank), Rune (#367: +116 MiB GPU on Windows, +28.6 MB, pre-release, takes no pull requests),
  GTK-only overlay, egui, native Wayland GTK on GNOME.
- **Self-hosted; parts swap by endpoint.** Settings names each speech server and the writing model by
  address on a standard protocol (Meta realtime ASR, OpenAI transcriptions, Chat Completions). Why: any
  provider or local server plugs in without code. Rejected: a plugin system (no faster, much more code).
