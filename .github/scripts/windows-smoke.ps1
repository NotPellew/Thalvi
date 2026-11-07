# Installs the built desktop app, starts it, opens Settings, and saves a screenshot of the desktop.
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

  'Windows before opening Settings:'
  [Windows]::Of($app.Id)
  $second = Start-Process $exe -PassThru -Wait
  "A second launch exited with $($second.ExitCode)."
  Wait-Until { [Windows]::Of($app.Id) -contains 'visible Thalvi settings' } 'the Settings window' 20
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
