---
name: verify-thalvi
description: Launch and drive Thalvi's real desktop app in an isolated Hyprland or GNOME sandbox, or a Windows test account, and capture evidence. Covers the overlay, native Settings, downloads, recognition, endings and delivery. Use to confirm user-visible desktop behavior beyond unit tests.
---

# Verify Thalvi

Thalvi is the desktop app (`crates/thalvi-app`, `crates/thalvi-desktop`). These
guides run the real app the way the user would, with no paid service and no
developer involved:

- [desktop.md](desktop.md): a chosen commit's debug build on a real Hyprland,
  nested and sandboxed, or on GNOME, for the overlay, Settings window,
  downloads, tray commands, recognition and delivery.
- [windows.md](windows.md): native Windows session prerequisites, Settings
  persistence and file-driven recognition and paste, with build identity and
  cleanup.
- [synthetic-audio.md](synthetic-audio.md): what synthetic audio proves, and
  when to ask the developer for a physical-audio check.

A run still needs the affected gates before pushing (AGENTS.md, Gates: `TESTS`
and `BUNDLE=false` narrow them). A desktop PR names each desktop it was used
on; CI and fixtures are not use.

`speech/asr/thalvi_asr.py` implements the realtime ASR protocol for a
self-hosted recognizer; [desktop.md](desktop.md) exercises it through a
`realtime` speech entry.
