# Delivery and actions

Getting the words into the window, and the actions a capture can run.

- **Delivery is a paste, held back from terminals.** Dictation and writing go on the clipboard and the bound window gets Ctrl+V (Ctrl+Shift+V in a known Linux terminal class); text may hold line breaks and tabs, no other control character. Unconfirmed text with a line break stays on the clipboard when the window is a terminal Thalvi pastes into: the Hyprland terminal classes, the Windows console and Windows Terminal. Submit pastes it after its confirmation shows the text. Hyprland sends the keys with `sendshortcut` and the keycode of V from the first layout (libxkbcommon).
  Why: a paste inserts line breaks without Enter, but a shell with bracketed paste off runs each pasted line, and recognized or written text arrives unread; GTK 3 ignores wtype's Ctrl keys, and `sendshortcut` keeps a key name's first keycode across layout changes.
  Rejected: typing with Shift+Enter for line breaks, typing single lines and pasting multi-line text (two paths), wtype keys, holding back all multi-line text (breaks emails in every app), holding back tabs (they run nothing), trusting bracketed paste.
- **Check, then paste.** Delivery re-verifies the bound window just before pressing keys,
  which go to whatever window is focused, except Hyprland's paste keys, which are addressed to it. Why: Enter and Windows
  input cannot be addressed to one window, so a switch in those milliseconds can misdirect them.
  Rejected: per-desktop accessibility or input protocols (much code for a rare race).
- **Enter waits 250 ms.** On Hyprland, submit waits 250 ms between the paste and Enter.
  Why: Muse Code 1.4.14 read a faster Enter as a new line while the overlay said sent;
  0 of 15 sent at up to 100 ms, 15 of 15 from 150 ms.
  Rejected: two wtype calls, a delay per application.
- **Scripted decisions, typed actions.** The demo matches configured phrases to
  browser and app actions; execution receives a typed action. Why: a later model
  can choose the same actions. Rejected for now: a plugin runtime, speculative
  interfaces and open-ended computer use before the scripted flow works.
- **Live Discord actions are disabled.** `discord_message` is refused before any host
  call and the message stays on the clipboard. Why: trusted input can reach an existing
  child editor outside the gate, and a key acknowledgement is no delivery receipt.
  Rejected: shipping the unsafe path, channel webhooks and private REST.
