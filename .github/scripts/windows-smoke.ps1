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

function Get-Status {
  $file = Join-Path $OutDir 'status.json'
  Start-Process $exe -ArgumentList 'status' -Wait -NoNewWindow -RedirectStandardOutput $file
  Get-Content $file -Raw
}

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
  Wait-Until { (Get-Status) -match '"class"' -and (Get-Status) -notmatch 'unavailable' } 'the app to answer status'
  "status: $(Get-Status)"

  Start-Process $exe
  Wait-Until { Get-Process thalvi-desktop -ErrorAction SilentlyContinue | Where-Object MainWindowTitle -eq 'Thalvi settings' } 'the Settings window'
  Start-Sleep -Seconds 3
  Save-Screen 'settings'
  if ($app.HasExited) { throw "the app exited with $($app.ExitCode)" }
  'The app is running and its Settings window is open.'
} finally {
  Save-Screen 'final'
  if (-not $app.HasExited) { Stop-Process -Id $app.Id -Force }
}
