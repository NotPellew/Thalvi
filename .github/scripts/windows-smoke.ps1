# Installs the built desktop app, checks its Whisper helpers, starts it, opens Settings, and saves a
# screenshot of the desktop.
param([Parameter(Mandatory)][string]$Installer, [Parameter(Mandatory)][string]$OutDir)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null

$config = Join-Path $env:APPDATA 'thalvi\desktop.json'
New-Item -ItemType Directory -Force (Split-Path $config) | Out-Null
Set-Content $config '{"server":"ws://127.0.0.1:1","stt":{"engine":"server"}}'

Start-Process $Installer -ArgumentList '/S' -Wait
$exe = Join-Path $env:LOCALAPPDATA 'Programs\Thalvi\thalvi-desktop.exe'
if (-not (Test-Path $exe)) { throw "the installer left no app at $exe" }

function Wait-Until([scriptblock]$Done, [string]$What, [int]$Seconds = 60) {
  $deadline = (Get-Date).AddSeconds($Seconds)
  while ((Get-Date) -lt $deadline) {
    if (& $Done) { return }
    Start-Sleep -Milliseconds 500
  }
  throw "timed out waiting for $What"
}

# The helpers with a small pinned Whisper model; the app's own Whisper Turbo files are 574 MB.
$whisper = Join-Path (Split-Path $exe) 'whisper'
foreach ($variant in 'cpu', 'vulkan') {
  if (-not (Test-Path "$whisper\$variant\whisper-server.exe")) { throw "the installer left no $variant Whisper helper" }
}
$files = Join-Path $env:RUNNER_TEMP 'whisper-smoke'
New-Item -ItemType Directory -Force $files | Out-Null
$pinned = @(
  @{ Name = 'ggml-tiny.en-q5_1.bin'; Sha = 'c77c5766f1cef09b6b7d47f21b546cbddd4157886b3b5d6d4f709e91e66c7c2b'
     Url = 'https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-tiny.en-q5_1.bin' },
  @{ Name = 'ggml-silero-v6.2.0.bin'; Sha = '2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987'
     Url = 'https://huggingface.co/ggml-org/whisper-vad/resolve/9ffd54a1e1ee413ddf265af9913beaf518d1639b/ggml-silero-v6.2.0.bin' }
)
foreach ($file in $pinned) {
  $path = Join-Path $files $file.Name
  Invoke-WebRequest -Uri $file.Url -OutFile $path
  if ((Get-FileHash -Algorithm SHA256 $path).Hash.ToLowerInvariant() -ne $file.Sha) { throw "$($file.Name) does not match its checksum" }
}
$wav = Join-Path $files 'phrase.wav'
# System.Speech is in Windows PowerShell, not in pwsh.
powershell.exe -NoProfile -Command @"
Add-Type -AssemblyName System.Speech
`$voice = New-Object System.Speech.Synthesis.SpeechSynthesizer
`$format = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo 16000, ([System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen), ([System.Speech.AudioFormat.AudioChannel]::Mono)
`$voice.SetOutputToWaveFile('$wav', `$format)
`$voice.Speak('The quick brown fox jumps over the lazy dog.')
`$voice.Dispose()
"@
if (-not (Test-Path $wav)) { throw 'no spoken phrase was generated' }

function Start-Helper([string]$Variant, [string[]]$Device) {
  $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
  $listener.Start(); $port = $listener.LocalEndpoint.Port; $listener.Stop()
  $log = Join-Path $OutDir "whisper-$Variant-$port"
  $process = Start-Process "$whisper\$Variant\whisper-server.exe" -PassThru -WindowStyle Hidden `
    -RedirectStandardOutput "$log.out" -RedirectStandardError "$log.err" -ArgumentList (@(
      '-m', (Join-Path $files 'ggml-tiny.en-q5_1.bin'), '--vad', '-vm', (Join-Path $files 'ggml-silero-v6.2.0.bin'),
      '--host', '127.0.0.1', '--port', $port, '-nt', '-t', '2') + $Device)
  @{ Process = $process; Port = $port; Log = $log }
}
function Test-Ready($Helper) {
  if ($Helper.Process.HasExited) { return $true }
  try { (Invoke-WebRequest "http://127.0.0.1:$($Helper.Port)/health" -UseBasicParsing -TimeoutSec 2).StatusCode -eq 200 } catch { $false }
}
function Get-Text($Helper) {
  $reply = curl.exe -s -m 120 -F "file=@$wav" -F response_format=json -F language=en "http://127.0.0.1:$($Helper.Port)/inference"
  ($reply | ConvertFrom-Json).text
}
function Assert-Transcribes($Helper, [string]$What) {
  Wait-Until { Test-Ready $Helper } "the $What to start" 60
  if ($Helper.Process.HasExited) { throw "the $What exited with $($Helper.Process.ExitCode): $(Get-Content "$($Helper.Log).err" -Raw)" }
  $text = Get-Text $Helper
  "The $What heard: $text"
  if ($text -notmatch 'quick brown fox') { throw "the $What transcribed '$text'" }
}

$cpu = Start-Helper 'cpu' @('-ng')
Assert-Transcribes $cpu 'CPU helper'
Stop-Process -Id $cpu.Process.Id -Force
$cpu.Process.WaitForExit()
$cpu = Start-Helper 'cpu' @('-ng')
Assert-Transcribes $cpu 'restarted CPU helper'

$vulkan = Start-Helper 'vulkan' @('-dev', '0')
Wait-Until { Test-Ready $vulkan } 'the Vulkan helper to start or exit' 60
if ($vulkan.Process.HasExited) {
  # STATUS_DLL_NOT_FOUND: no vulkan-1.dll, which the graphics driver installs.
  "The Vulkan helper exited with $('0x{0:X8}' -f $vulkan.Process.ExitCode)."
} else {
  'The Vulkan helper started: ' + ((Get-Content "$($vulkan.Log).err") -match 'ggml_vulkan|using Vulkan|no GPU found' -join ' / ')
  Stop-Process -Id $vulkan.Process.Id -Force
}
Assert-Transcribes $cpu 'CPU helper after the Vulkan attempt'
Stop-Process -Id $cpu.Process.Id -Force

Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class Windows {
  delegate bool Visit(IntPtr window, IntPtr data);
  [DllImport("user32.dll")] static extern bool EnumWindows(Visit visit, IntPtr data);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr window, StringBuilder text, int size);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
  public static List<string> Of(uint wanted) {
    var found = new List<string>();
    EnumWindows((window, data) => {
      uint process;
      GetWindowThreadProcessId(window, out process);
      if (process == wanted) {
        var text = new StringBuilder(256);
        GetWindowText(window, text, 256);
        found.Add((IsWindowVisible(window) ? "visible " : "hidden ") + text);
      }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
'@

function Save-Screen([string]$Name) {
  Add-Type -AssemblyName System.Windows.Forms, System.Drawing
  $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
  $bitmap.Save((Join-Path $OutDir "$Name.png"))
  $graphics.Dispose()
  $bitmap.Dispose()
}

$app = Start-Process $exe -PassThru `
  -RedirectStandardOutput (Join-Path $OutDir 'app.log') -RedirectStandardError (Join-Path $OutDir 'app.err')
try {
  $log = Join-Path $OutDir 'app.log'
  Wait-Until { (Get-Content $log -Raw -ErrorAction SilentlyContinue) -match 'Transcribing through' } 'the app to finish starting'
  Get-Content $log

  Wait-Until { [Windows]::Of($app.Id) -contains 'hidden Thalvi settings' } 'the app to create its Settings window' 30
  # On the runner a relaunch soon after start can go unanswered, so this asks again.
  Wait-Until {
    Start-Process $exe -Wait
    Start-Sleep -Seconds 1
    [Windows]::Of($app.Id) -contains 'visible Thalvi settings'
  } 'the Settings window' 45
  Start-Sleep -Seconds 3
  Save-Screen 'settings'
  if ($app.HasExited) { throw "the app exited with $($app.ExitCode)" }
  'The app is running and its Settings window is open.'
} finally {
  'Windows at the end:'
  [Windows]::Of($app.Id)
  Save-Screen 'final'
  if (-not $app.HasExited) { Stop-Process -Id $app.Id -Force }
}
