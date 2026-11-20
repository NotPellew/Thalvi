# Model downloads

Choosing a wake or speech model that is not on the computer downloads it; the
user sees that a download is running, then **Ready**, or the reason it failed
in plain words, and can try again with **Download**.

Spec: none — no runner drives the real app; download, hashing and timeouts are unit-tested in `crates/thalvi-desktop`

## Sub-features

- `downloads-start` a start fetches only the selected wake head, the mel and embedding models, the four `kws_gigaspeech_3m.*` ending-spotter files and the active on-device speech model.
- `downloads-wake` Listening → **Wake model (dictation)**: choosing a model downloads it; **Download** retries.
- `downloads-speech` Recognition → speech model cards with size, **Download**, **Checking files…**, **Downloading… N%** and **Copy report** on failure.
- `downloads-failure` a failure names the host and says **Check the internet connection and try again.**, beside the row and in the error bar.

## How to get to it (user POV)

- Settings → **Listening** → **Dictation wake word** → **Wake model (dictation)**.
- Settings → **Recognition** → **On this computer**; Settings → **Chat model** → **Download chat model**.
- `thalvi-desktop models` and `thalvi-desktop model-download <wake|speech|chat> NAME`, which show nothing on screen.

## Driving it with the rig

Preconditions:

- A run started **without** `--offline`, with only the files `downloads-start` names copied in, so `hey_mycroft` and `hey_jarvis` are absent (`ctl models` shows `downloaded: false`).
- `scripts/throttle-proxy.py 18081 0` started with `setsid`; record the PID that `ss -ltnp` shows on port 18081 (the launched PID exits). The app started with `--env HTTPS_PROXY=http://127.0.0.1:18081`.

- **Start clean.** The app reaches `listening` with recognizer `ready` and its log has no `Downloading` line.
- **Choose a model.** `$H ctl $R show listening`, focus Settings, 12 Tabs to **Wake model (dictation)**, `Down`. The row shows **Hey Mycroft — Not downloaded**, **Not downloaded**, **Download** and **Applied.**; the overlay shows **LOADING / Downloading the wake word model.**
- **Fail.** Tab to **Download**, `Return`. The row reads **Downloading…**; 2 s later it returns to **Not downloaded** with `Could not download the wake model hey_mycroft_v0.1.onnx: could not connect to github.com. Check the internet connection and try again.` under it and in the error bar. `$MET` has `desktop_wake_load` `model: hey_mycroft`, `source: settings_download`, `outcome: error`.
- **Succeed.** Stop the proxy by its PID, start `throttle-proxy.py 18081 250000` on the same port, `Shift+Tab` back to **Download**, `Return`. **Downloading…**, then **Hey Mycroft — Ready** and **Ready** (3.7 s at that rate).
- **Proof.** `ctl models` shows `hey_mycroft` `downloaded: true`; `$MET` has `desktop_wake_load` `outcome: ok`, `source: settings_download`.

## Layout and timing

Window sizes 400×680, 780×680 and 1280×900, with `settings-boxes` (README,
Layout checks).

| Screen (checked when it first appears) | Layout: elements checked | Wait: action → awaited element, normal, limit | Fallback signal |
|---|---|---|---|
| Listening page (Choose a model) | `heading=Listening`, `switch=Listening`, `combobox=Microphone`, `switch=Wake words`, `combobox=Wake model (dictation)`, `heading=Hold keys` | none: loaded with Settings | none |
| Wake model row (Fail, Succeed) | none beyond the page | `Return` on **Download** → **Downloading…** in the row, 0.23 s, 1 s | the row stays on **Not downloaded** with no message |
| Download overlay (Choose a model) | the overlay lies wholly inside the screen at top centre, by screenshot | `Down` on the drop-down → **Downloading the wake word model.** in the overlay, 0.22 s (one run), 1 s | none |

A transfer's length depends on size and network, so it has no time limit
here; the app's own limit (10 s to connect, then 120 s plus one second per
500 kB) ends with **the download timed out. Check the internet connection
and try again.**

Measured 2026-11-20: **Downloading…** 0.02, 0.23, 0.04 s after `Return`.
All three sizes pass for the Listening page.

## Gotchas

- `ctl model-download` downloads and replies but shows no notice; use the Settings path to see what the user sees.
- After a failed **Download**, no control shows focus and `Return` does nothing; `Tab` goes on to **Details**. `Shift+Tab` returns to **Download**.
- While a download started by choosing a model runs, the row says **Not downloaded** with **Download** enabled; only the overlay says a download is running.
- Cached files are accepted by their pinned size, not rehashed; a damaged file of the right size passes the check.
- A refused tunnel (rate `0`) fails after 2 s, long enough to see **Downloading…**.
- The proxy setting is fixed when the app starts; change what listens on the port, not the app's environment.
