# Installs the built desktop app, checks its Whisper helpers, starts it, opens Settings, feeds a
# dictation fixture into Notepad, and saves screenshots of the desktop.
param([Parameter(Mandatory)][string]$Installer, [Parameter(Mandatory)][string]$OutDir)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null

$config = Join-Path $env:APPDATA 'thalvi\desktop.json'
New-Item -ItemType Directory -Force (Split-Path $config) | Out-Null
Set-Content $config '{"stt":{"entries":[{"kind":"on_device","id":"ci","name":"On this computer","active":true,"model":"nemotron_160ms"}]},"recordings":{"metrics":true}}'

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
  [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr window, int command);
  [DllImport("user32.dll")] static extern bool IsIconic(IntPtr window);
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] static extern bool AttachThreadInput(uint sourceThread, uint targetThread, bool attach);
  [DllImport("user32.dll")] static extern bool BringWindowToTop(IntPtr window);
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
  public static bool Focus(uint wanted) {
    var found = new List<IntPtr>();
    EnumWindows((window, data) => {
      uint process;
      GetWindowThreadProcessId(window, out process);
      if (process == wanted && IsWindowVisible(window)) found.Add(window);
      return true;
    }, IntPtr.Zero);
    if (found.Count == 0) return false;
    IntPtr window = found[0];
    if (IsIconic(window)) ShowWindow(window, 9);
    uint ignored;
    uint foregroundThread = GetWindowThreadProcessId(GetForegroundWindow(), out ignored);
    uint targetThread = GetWindowThreadProcessId(window, out ignored);
    uint current = GetCurrentThreadId();
    if (foregroundThread != 0) AttachThreadInput(current, foregroundThread, true);
    if (targetThread != 0) AttachThreadInput(current, targetThread, true);
    BringWindowToTop(window);
    bool ok = SetForegroundWindow(window);
    if (targetThread != 0) AttachThreadInput(current, targetThread, false);
    if (foregroundThread != 0) AttachThreadInput(current, foregroundThread, false);
    return ok;
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

function Read-Thalvi {
  param([Parameter(ValueFromRemainingArguments = $true)][string[]]$ArgumentList)
  $output = & $exe @ArgumentList 2>&1 | Out-String
  $line = @($output -split '\r?\n' | Where-Object { $_.Trim().StartsWith('{') } | Select-Object -Last 1)
  if (-not $line) { throw "thalvi-desktop $($ArgumentList -join ' ') printed no JSON: $output" }
  return $line | ConvertFrom-Json
}
function Test-FileFeedReady($State) {
  if ($State.ok -ne $true -or $State.paused -eq $true -or $State.recognizer.model -ne 'nemotron_160ms') {
    return $false
  }
  return $State.recognizer.status -eq 'ready' -or
    ($State.state -eq 'failed' -and $State.failure.headline -match '^(No microphone found|Microphone not connected)')
}

function Invoke-FileDelivery {
  . (Join-Path $PSScriptRoot 'e2e/windows/transcript.ps1')
  $PSNativeCommandUseErrorActionPreference = $false
  $script:owned = $null
  $script:actual = ''
  $script:readyState = ''
  $script:readError = ''

  function Read-NotepadText([int]$ProcessId, [switch]$PrepareFocus) {
    $reader = Join-Path $PSScriptRoot 'e2e/windows/read-notepad.ps1'
    $args = @("-ProcessId", $ProcessId)
    if ($PrepareFocus) { $args += "-PrepareFocus" }
    $output = & powershell.exe -NoProfile -NonInteractive -File $reader @args 2>&1 | Out-String
    $line = @($output -split '\r?\n' | Where-Object { $_.Trim().StartsWith('{') } | Select-Object -Last 1)
    if (-not $line) { return [pscustomobject]@{ ok = $false; text = ''; error = $output } }
    return $line | ConvertFrom-Json
  }

  try {
    $wav = Join-Path $PSScriptRoot 'e2e/fixtures/dictation-feed.wav'
    $expectedFile = Join-Path $PSScriptRoot 'e2e/fixtures/dictation.txt'
    if (-not (Test-Path -LiteralPath $wav)) { throw "the dictation fixture is missing: $wav" }
    if (-not (Test-Path -LiteralPath $expectedFile)) { throw "the expected transcript is missing: $expectedFile" }
    $wav = (Resolve-Path -LiteralPath $wav).Path
    $expected = Get-Content -LiteralPath $expectedFile -Raw

    try {
      Wait-Until {
        $state = Read-Thalvi state
        $script:readyState = $state | ConvertTo-Json -Compress
        return $state.ok -eq $true -and $state.state -ne 'loading'
      } 'the app to finish loading Nemotron' 1500
    } catch {
      if ($_.Exception.Message -notlike 'timed out waiting for*') { throw }
      throw "timed out waiting for the app to finish loading Nemotron. Last state: $script:readyState"
    }
    $download = Read-Thalvi model-download speech nemotron_160ms
    if ($download.ok -ne $true) {
      $reason = [string]$download.error
      if ($reason -notlike '*already being downloaded*') {
        throw "thalvi-desktop model-download speech nemotron_160ms was refused: $reason"
      }
      Wait-Until {
        $listed = Read-Thalvi models
        $model = @($listed.models) | Where-Object { $_.kind -eq 'speech' -and $_.name -eq 'nemotron_160ms' } | Select-Object -First 1
        $model.downloaded -eq $true
      } 'the Nemotron model to finish downloading' 1500
    }
    try {
      Wait-Until {
        $state = Read-Thalvi state
        $script:readyState = $state | ConvertTo-Json -Compress
        Test-FileFeedReady $state
      } 'the Nemotron recognizer to be ready' 300
    } catch {
      if ($_.Exception.Message -notlike 'timed out waiting for*') { throw }
      throw "timed out waiting for the Nemotron recognizer to be ready. Last state: $script:readyState"
    }

    $seen = @(Get-Process -Name notepad -ErrorAction SilentlyContinue | ForEach-Object Id)
    Start-Process notepad.exe | Out-Null
    Wait-Until {
      $candidate = @(Get-Process -Name notepad -ErrorAction SilentlyContinue) |
        Where-Object { $seen -notcontains $_.Id -and $_.ProcessName -eq 'notepad' -and ([Windows]::Of([uint32]$_.Id) -match 'visible ') } |
        Select-Object -First 1
      if ($null -eq $candidate) { return $false }
      $script:owned = $candidate
      return $true
    } 'an owned Notepad window' 30
    try {
      Wait-Until {
        [void][Windows]::Focus([uint32]$script:owned.Id)
        $read = Read-NotepadText $script:owned.Id -PrepareFocus
        if ($read.ok -eq $true) {
          if (-not [string]::IsNullOrWhiteSpace($read.text)) { throw "Notepad was not empty before dictation: '$($read.text)'" }
          return $true
        }
        $script:readError = [string]$read.error
        return $false
      } "Notepad's focused editor to be readable" 30
    } catch {
      if ($_.Exception.Message -notlike 'timed out waiting for*') { throw }
      throw "Could not read Notepad's focused editor. Last reader error: $script:readError"
    }
    Save-Screen 'notepad'

    $fed = Read-Thalvi feed $wav
    if ($fed.ok -ne $true) { throw "thalvi-desktop feed was refused: $($fed | ConvertTo-Json -Compress)" }
    try {
      Wait-Until {
        $read = Read-NotepadText $script:owned.Id
        if ($read.ok -eq $true) {
          $script:actual = [string]$read.text
          $script:readError = ''
          return (ConvertTo-DictationComparable $script:actual) -eq (ConvertTo-DictationComparable $expected)
        }
        $script:readError = [string]$read.error
        return $false
      } 'Notepad to show the dictation' 180
    } catch {
      if ($_.Exception.Message -notlike 'timed out waiting for*') { throw }
      try { Assert-DictationText -Actual $script:actual -Expected $expected } catch {
        $detail = $_.Exception.Message
        if ($script:readError) { $detail = "$detail Last reader error: $script:readError" }
        throw $detail
      }
      throw
    }
    Assert-DictationText -Actual $script:actual -Expected $expected
  } finally {
    try {
      $metrics = Join-Path $env:LOCALAPPDATA 'thalvi\recordings\metrics.jsonl'
      $kept = Join-Path $OutDir 'metrics.jsonl'
      if (Test-Path -LiteralPath $metrics) {
        Copy-Item -LiteralPath $metrics -Destination $kept -Force
      } else {
        Set-Content -LiteralPath $kept -Value 'metrics.jsonl was not written' -Encoding utf8
      }
      Set-Content -LiteralPath (Join-Path $OutDir 'notepad.txt') -Value $script:actual -Encoding utf8
      Save-Screen 'delivery'
    } finally {
      if ($null -ne $script:owned) {
        Stop-Process -Id $script:owned.Id -Force -ErrorAction SilentlyContinue
        try { [void]$script:owned.WaitForExit(5000) } catch {}
      }
      $beep = 'The file-input smoke test does not measure start-beep, end-beep, or hold-release timing.'
      Write-Output $beep
      if ($env:GITHUB_STEP_SUMMARY) {
        Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value $beep -Encoding utf8
      }
    }
  }
}

$app = Start-Process $exe -PassThru `
  -RedirectStandardOutput (Join-Path $OutDir 'app.log') -RedirectStandardError (Join-Path $OutDir 'app.err')
try {
  $log = Join-Path $OutDir 'app.log'
  Wait-Until { (Get-Content $log -Raw -ErrorAction SilentlyContinue) -match 'Transcribing through' } 'the app to finish starting'
  Get-Content $log

  Invoke-FileDelivery
  Wait-Until {
    if ($app.HasExited) { throw "the app exited with $($app.ExitCode)" }
    if ([Windows]::Of($app.Id) -contains 'visible Thalvi settings') { return $true }
    $relaunch = Start-Process $exe -PassThru
    if (-not $relaunch.WaitForExit(5000)) { $relaunch.Kill() }
    [Windows]::Of($app.Id) -contains 'visible Thalvi settings'
  } 'the Settings window' 45
  Save-Screen 'settings'
  if ($app.HasExited) { throw "the app exited with $($app.ExitCode)" }
  'The app is running and its Settings window is open.'
} finally {
  'Windows at the end:'
  [Windows]::Of($app.Id)
  Save-Screen 'final'
  if (-not $app.HasExited) { Stop-Process -Id $app.Id -Force }
}
