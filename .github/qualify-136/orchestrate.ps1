# Runs as the runner's admin: creates a throwaway user, logs it in and out through loopback RDP,
# and runs in-session.ps1 steps inside that user's own desktop session.
param([Parameter(Mandatory)][string]$Installer, [Parameter(Mandatory)][string]$OutDir, [Parameter(Mandatory)][string]$User)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$user = $User
$qa = 'C:\qa'
$shared = "$qa\out"
New-Item -ItemType Directory -Force $shared | Out-Null
icacls $qa /grant '*S-1-5-32-545:(OI)(CI)M' | Out-Null
Copy-Item $Installer "$qa\thalvi-setup.exe"
Copy-Item (Join-Path $PSScriptRoot 'in-session.ps1') "$qa\in-session.ps1"

function Log([string]$Text) {
  $line = "$(Get-Date -Format HH:mm:ss.fff) $Text"
  Write-Host $line
  Add-Content "$OutDir\orchestrate.log" $line
}

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
function Save-Console([string]$Name) {
  try {
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    $bitmap.Save("$OutDir\console-$Name.png")
    $graphics.Dispose(); $bitmap.Dispose()
  } catch { Log "console screenshot failed: $_" }
}

Log "windows: $((Get-CimInstance Win32_OperatingSystem).Caption) $([Environment]::OSVersion.Version)"
Log "runner session: $((Get-Process -Id $PID).SessionId)"

$chars = [char[]]'abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'
$password = -join (1..20 | ForEach-Object { $chars | Get-Random }) + '!7a'
$secure = ConvertTo-SecureString $password -AsPlainText -Force
New-LocalUser -Name $user -Password $secure -PasswordNeverExpires -AccountNeverExpires | Out-Null
Add-LocalGroupMember -SID 'S-1-5-32-555' -Member $user
Log "created $user (a standard user, in Remote Desktop Users)"

Set-ItemProperty 'HKLM:\System\CurrentControlSet\Control\Terminal Server' fDenyTSConnections 0
Start-Service TermService
$policy = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\CredentialsDelegation'
New-Item -Force $policy | Out-Null
foreach ($name in 'AllowSavedCredentials', 'AllowSavedCredentialsWhenNTLMOnly') {
  Set-ItemProperty $policy $name 1 -Type DWord
  New-Item -Force "$policy\$name" | Out-Null
  Set-ItemProperty "$policy\$name" '1' 'TERMSRV/*'
}
Set-ItemProperty $policy ConcatenateDefaults_AllowSaved 1 -Type DWord
Set-ItemProperty $policy ConcatenateDefaults_AllowSavedNTLMOnly 1 -Type DWord
New-Item -Force 'HKCU:\Software\Microsoft\Terminal Server Client' | Out-Null
Set-ItemProperty 'HKCU:\Software\Microsoft\Terminal Server Client' AuthenticationLevelOverride 0 -Type DWord
cmdkey /generic:TERMSRV/127.0.0.2 /user:"$env:COMPUTERNAME\$user" /pass:$password | Out-Null

function Session-Explorer {
  Get-Process explorer -IncludeUserName -ErrorAction SilentlyContinue | Where-Object { $_.UserName -like "*\$user" } | Select-Object -First 1
}

$script:logons = 0
function Sign-In {
  $script:logons++
  $n = $script:logons
  Log "L${n}: connecting"
  $script:mstsc = Start-Process mstsc -ArgumentList '/v:127.0.0.2', '/w:1280', '/h:800' -PassThru
  $deadline = (Get-Date).AddSeconds(240)
  while ((Get-Date) -lt $deadline) {
    $explorer = Session-Explorer
    if ($explorer) {
      Log "L${n}: logged in, session $($explorer.SessionId), explorer pid $($explorer.Id) started $($explorer.StartTime.ToString('HH:mm:ss.fff'))"
      return $explorer.SessionId
    }
    Start-Sleep -Milliseconds 500
  }
  Save-Console "L$n-no-logon"
  quser 2>&1 | ForEach-Object { Log "quser: $_" }
  throw "L${n}: $user never got a desktop"
}

function Sign-Out([int]$Session) {
  Log "L$($script:logons): logging off session $Session"
  logoff.exe $Session
  $deadline = (Get-Date).AddSeconds(120)
  while ((Get-Date) -lt $deadline) {
    if (-not (Session-Explorer) -and -not (quser 2>$null | Select-String $user)) {
      if ($script:mstsc -and -not $script:mstsc.HasExited) { Stop-Process -Id $script:mstsc.Id -Force -ErrorAction SilentlyContinue }
      Log "logged off"
      return
    }
    Start-Sleep -Milliseconds 500
  }
  throw "session $Session did not end"
}

function In-Session([string]$Step, [int]$Seconds = 300) {
  $label = "L$($script:logons)-$Step"
  $name = "qa-$label"
  # conhost --headless runs PowerShell with no console window, so nothing of ours takes focus.
  $action = New-ScheduledTaskAction -Execute 'C:\Windows\System32\conhost.exe' -Argument "--headless powershell.exe -NoProfile -ExecutionPolicy Bypass -File $qa\in-session.ps1 -Step $Step -Label $label"
  $principal = New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\$user" -LogonType Interactive -RunLevel Limited
  $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 15)
  Register-ScheduledTask -TaskName $name -Action $action -Principal $principal -Settings $settings -Force | Out-Null
  Start-ScheduledTask -TaskName $name
  Log "step $label started"
  $done = "$shared\$label.done"
  $deadline = (Get-Date).AddSeconds($Seconds)
  while (-not (Test-Path $done)) {
    if ((Get-Date) -gt $deadline) { Save-Console "$label-timeout"; throw "step $label did not finish" }
    Start-Sleep -Milliseconds 500
  }
  Unregister-ScheduledTask -TaskName $name -Confirm:$false
  Get-Content "$shared\$label.log" | ForEach-Object { Log "  [$label] $_" }
  $result = Get-Content $done -Raw
  if ($result -notmatch '^ok') { $script:failures += "${label}: $result"; Log "STEP FAILED ${label}: $result" }
}

$failed = $null
$script:failures = @()
try {
  # L1: install, start by hand, turn the login switch on.
  $s = Sign-In
  In-Session 'install'
  Sign-Out $s

  # L2: the Run entry should start one tray instance with --login, no Settings, no focus.
  $s = Sign-In
  In-Session 'observe-started'
  In-Session 'approved-off'
  Sign-Out $s

  # L3: Task Manager's toggle is off, so nothing starts; a manual start must not override it.
  $s = Sign-In
  In-Session 'observe-absent'
  In-Session 'manual-respects-approved'
  Sign-Out $s

  # L4: the switch is off, so nothing starts. Then turn it on again and uninstall.
  $s = Sign-In
  In-Session 'observe-absent'
  In-Session 'uninstall'
  Sign-Out $s

  # L5: after uninstalling, nothing starts and nothing complains.
  $s = Sign-In
  In-Session 'observe-absent'
  Sign-Out $s
} catch {
  $failed = $_
  Log "FAILED: $_"
  Save-Console 'failure'
} finally {
  Copy-Item "$shared\*" $OutDir -Recurse -Force -ErrorAction SilentlyContinue
  cmdkey /delete:TERMSRV/127.0.0.2 | Out-Null
}
if ($failed) { throw $failed }
if ($script:failures.Count) { $script:failures | ForEach-Object { Log "failed: $_" }; throw "$($script:failures.Count) step(s) failed" }
Log 'all steps passed'
