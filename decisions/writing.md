# Writing

The chat model: writing, cleanup and prompts.

- **Chat Completions first.** Writing and the extra-recognizer merge speak it. Why: one adapter
  covers Meta, Ollama, vLLM and llama.cpp.
- **Writing is one direct chat call, words only.** "write that out" or the write key sends the recognized words to an
  OpenAI-compatible endpoint from Settings and pastes what streams back, line breaks included; no context, no Enter, 15 s at most;
  reasoning shows as thinking, never pasted, and the request says nothing about thinking; any failure pastes nothing and puts the words
  on the clipboard. An explicit write uses `writing.prompt` (or an opening or app style); dictation cleanup, when on, uses its own
  built-in prompt. Why: no server needed, unsaid text never sent unread, strict endpoints refuse unknown fields. Rejected: the Thalvi server, write-and-send, sending `enable_thinking`.
- **Dictation cleanup is opt-in.** `writing.clean_dictation`, stored only when on and refused without an endpoint, cleans each paste or submit after any final pass or merge with one built-in prompt and pastes the whole reply; a submit is cleaned before its confirmation, so that confirmation shows the text that will be sent; a failure or no reply within 3 s pastes the raw words and says so, and a cancel pastes nothing. Tasks and explicit writes are never cleaned.
  Why: tidier dictation must never cost the user their words.
  Rejected: cleaning by default, a user-editable cleanup prompt, pasting a partial reply.
- **Local rules format dictated numbers as digits.** `writing.numbers_as_digits` defaults to on for English and German paste, submit, and write. One deterministic formatter runs after the ending is stripped and the final transcript settles, before cleanup and submit confirmation.
  Tasks, live partials, word-list tokens, and other languages pass through unchanged.
  Why: recognizers vary in how they write numbers. Local rules produce consistent digits offline, including without a chat endpoint.
  Rejected: relying on the cleanup prompt or encoding number style in the word list.
- **Local chat is a pinned prebuilt child.** Settings installs llama.cpp b12255 and Qwen3 4B Q4_K_M (Apache-2.0); the runtime's own Vulkan device check chooses GPU or CPU, with a CPU override. Writing and merging lazily share one loopback child with a fresh key, deterministic sampling and thinking disabled in the template; loading is cancellable and measured separately. One crash gets one restart on the next use, and quitting stops it.
  Why: the 4B kept EN/DE/ES meaning in the fixed comparison and CPU generation stayed below 1.6 s on the desktop; the smaller candidates invented text or translated it. Windows laptop acceptance remains required.
  Rejected: user builds, containers, OS services, a reasoning-budget flag alone, and downloading into a remote server.
- **The writing prompt is a stored override of one built-in default.** `writing.prompt` is the system message; it is
  saved only when it differs from the default, and a blank one is refused where the config loads. Why: users who never
  edited it get a better default later, and Settings, the agent surface and the file share one check. Rejected:
  storing the default text, a prompt without an endpoint.
- **Opening phrases, then the bound app, pick the writing prompt.** An explicit write whose words start with a `writing.openings` phrase as whole words gets its prompt (longest wins); else the first `writing.apps` style whose app (Hyprland class, Windows executable) and optional title fragment match the window bound at wake; else `writing.prompt`. Never combined; words go as said; app and title only match locally, never sent or recorded; GNOME names no window, so styles don't apply there.
  Why: small models follow one long prompt unevenly, and local matching adds no call. Rejected: dropping the opening from the message, a switch per rule, a routing model call, combining an opening with an app style, one prompt with a section per kind.
