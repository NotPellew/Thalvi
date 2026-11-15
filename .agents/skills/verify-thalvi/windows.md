# Native Windows checks

Use an active, unlocked desktop in the designated test account. SSH access or
a headless agent in session 0 is not an interactive desktop. Check `query user`
and the session IDs of the app and driver before sending input; do not drive
another account's app. From SSH, an interactive task with that account's
`InteractiveToken` can launch the GUI driver. A task running in session 0
cannot establish desktop use. Retain and remove only this run's task names.

## Build identity and isolation

Record the executable's SHA256 and version. An existing installed release
proves that binary's behavior, not the current checkout. To claim a new commit,
use its Windows build or CI artifact and retain the build/commit provenance.
An already installed executable also does not establish installer behavior;
the repository's `.github/scripts/windows-smoke.ps1` tests an installer.

For an installed app, the usual executable is
`$env:LOCALAPPDATA\Programs\Thalvi\thalvi-desktop.exe`. Its `state` and `status`
commands query the current user's app without opening a new one. Record its
initial state and the hash of `$env:APPDATA\thalvi\desktop.json`. If a resident
app must be stopped, restore it afterwards. Keep the test config separate:
the executable accepts a **config-file path as its sole argument**, not a
`--config` option. Wait for the previous process to exit before launching it;
the single-instance path otherwise talks to the resident app.

```powershell
$exe = Join-Path $env:LOCALAPPDATA 'Programs\Thalvi\thalvi-desktop.exe'
query user
Get-FileHash $exe -Algorithm SHA256
(Get-Item $exe).VersionInfo.ProductVersion
& $exe state
Get-FileHash (Join-Path $env:APPDATA 'thalvi\desktop.json') -Algorithm SHA256
```

Use an existing cached on-device model, no writing endpoint or external
actions, and synthetic speech. Disable microphone audio recordings and sounds;
physical audio is outside this verification scope. Retain metrics and test text.
Keep the original login/menu switches unchanged. Do not print credentials
from the original settings.

## Settings

Run `show overlay` on the test-owned app, then capture its fully rendered
Settings window. Use that build's visible controls: the installed 0.1.3
release tested here has **Save**, while the current source uses autosave.
Select another look, save if required, and require both the changed window
and `look` in the test config. Quit, wait for its PID to exit, restart with
the same config path, reopen Settings and capture the persisted choice.

For native screenshots and coordinate input, set process DPI awareness before
obtaining screen bounds or coordinates (`user32!SetProcessDPIAware` sufficed
in the tested single-monitor session). Check the image's actual pixel size.
Verify the target window's process ID and foreground handle immediately
before input, and release every pressed input in `finally`. Stop on focus
drift. A screenshot taken with an unaware driver can crop the screen and hide
the Save footer. Wait for the screenshot task to finish before copying its
PNG; copying a file still being written can produce an invalid image.

## Recognition and paste destination

Keep the scratch destination's event loop independent from the process that
waits for Thalvi commands. A PowerShell WinForms driver that blocks its own
UI thread waiting for the CLI can prevent the destination from processing
paste. A separate process running `Form.ShowDialog()` worked. Its multiline
TextBox logged `TextChanged` to a test-owned file and remained responsive while
another process issued commands.

The destination process can use this minimal form, with `$readback` set to an
absolute file in the run's evidence directory. Keep this process running
through delivery; do not run the `feed` command on its UI thread.

```powershell
Add-Type -AssemblyName System.Windows.Forms
$form = New-Object Windows.Forms.Form
$form.Text = 'Thalvi verification scratch'
$form.Width = 650; $form.Height = 330
$box = New-Object Windows.Forms.TextBox
$box.Multiline = $true; $box.Dock = 'Fill'
$form.Controls.Add($box)
$box.Add_TextChanged({ $box.Text | Set-Content $readback })
try { $form.ShowDialog() | Out-Null } finally { $form.Dispose() }
```

Use the desktop guide's PCM16 file fixture: **Hey Thalvi**, silence for the
ready cue, a distinctive sentence, then **Finish dictation**. In the test
session, focus the scratch TextBox and verify its foreground process. Invoke
`feed <absolute-WAV-path>` from a separate process. Its reply can still say
`capturing`; wait boundedly for the expected text in the destination and a
settled app state. Retain a screenshot, the TextBox's readback, and the new
capture/delivery metrics. Do not accept a `delivered` state alone as proof that
the destination received text.

This exercises the real recognizer and Windows paste path using file input.
It proves neither the physical microphone nor the physical hold shortcut:
the Windows hook deliberately ignores injected key events. Report that boundary;
do not add physical audio or a supervised headset session to the plan. Synthetic
key-state tests and CLI hold controls are separate evidence from hardware keys.

## Cleanup

Close the owned scratch process and test app, verify their recorded PIDs are
gone, and retain evidence before removing temporary task definitions. Check
the original config hash is unchanged, then restore the original resident
app and its initial listening/keyboard/paused mode. A task can remain marked
running while a restored child app lives; distinguish that intentional
restoration from a leaked test process. Do not stop another session's app.
