# One step of the #136 qualification, run inside the throwaway user's own desktop session.
# Writes C:\qa\out\<Label>.log, .json and .png files, then <Label>.done holding "ok" or the failure.
param([Parameter(Mandatory)][string]$Step, [Parameter(Mandatory)][string]$Label)
$ErrorActionPreference = 'Stop'
$out = 'C:\qa\out'
$logFile = "$out\$Label.log"
$session = (Get-Process -Id $PID).SessionId
$exe = Join-Path $env:LOCALAPPDATA 'Programs\Thalvi\thalvi-desktop.exe'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$approvedKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run'
$sentinel = "$out\sentinel.txt"
$facts = [ordered]@{ step = $Step; label = $Label; session = $session; user = $env:USERNAME }
$problems = New-Object System.Collections.Generic.List[string]

function Log([string]$Text) { "$(Get-Date -Format HH:mm:ss.fff) $Text" | Out-File $logFile -Append -Encoding utf8 }
function Check([bool]$Ok, [string]$What) {
  if ($Ok) { Log "PASS $What" } else { Log "FAIL $What"; $problems.Add($What) }
}

Add-Type -AssemblyName System.Windows.Forms, System.Drawing, UIAutomationClient, UIAutomationTypes
Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class Win {
  delegate bool Visit(IntPtr window, IntPtr data);
  [DllImport("user32.dll")] static extern bool EnumWindows(Visit visit, IntPtr data);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr window, StringBuilder text, int size);
  public static string Title(IntPtr window) { var t = new StringBuilder(256); GetWindowText(window, t, 256); return t.ToString(); }
  public static uint Owner(IntPtr window) { uint p; GetWindowThreadProcessId(window, out p); return p; }
  public static List<IntPtr> Of(uint wanted) {
    var found = new List<IntPtr>();
    EnumWindows((window, data) => { if (wanted == 0 || Owner(window) == wanted) found.Add(window); return true; }, IntPtr.Zero);
    return found;
  }
}
'@

function Windows-Of([int]$ProcessId) {
  [Win]::Of([uint32]$ProcessId) | ForEach-Object { $(if ([Win]::IsWindowVisible($_)) { 'visible ' } else { 'hidden ' }) + [Win]::Title($_) }
}
function Visible-Titles {
  [Win]::Of(0) | Where-Object { [Win]::IsWindowVisible($_) -and [Win]::Title($_) } | ForEach-Object {
    $p = Get-Process -Id ([Win]::Owner($_)) -ErrorAction SilentlyContinue
    "$([Win]::Title($_)) [$($p.ProcessName)]"
  }
}
function Foreground {
  $w = [Win]::GetForegroundWindow()
  if ($w -eq [IntPtr]::Zero) { return 'none' }
  $p = Get-Process -Id ([Win]::Owner($w)) -ErrorAction SilentlyContinue
  "$($p.ProcessName)#$($p.Id) '$([Win]::Title($w))'"
}
function Save-Screen([string]$Name) {
  try {
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    $bitmap.Save("$out\$Label-$Name.png")
    $graphics.Dispose(); $bitmap.Dispose()
  } catch { Log "screenshot $Name failed: $_" }
}
function Wait-Until([scriptblock]$Done, [string]$What, [int]$Seconds = 60) {
  $deadline = (Get-Date).AddSeconds($Seconds)
  while ((Get-Date) -lt $deadline) {
    $value = & $Done
    if ($value) { return $value }
    Start-Sleep -Milliseconds 250
  }
  throw "timed out after ${Seconds}s waiting for $What"
}
function Thalvi-Processes { @(Get-Process thalvi-desktop -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq $session }) }
function Command-Lines { @(Get-CimInstance Win32_Process -Filter "Name='thalvi-desktop.exe'" | Where-Object { $_.SessionId -eq $session } | ForEach-Object { $_.CommandLine }) }
function Run-Value { (Get-ItemProperty $runKey -ErrorAction SilentlyContinue).Thalvi }
function Approved-Value {
  $v = (Get-ItemProperty $approvedKey -ErrorAction SilentlyContinue).Thalvi
  if ($null -eq $v) { return $null }
  ($v | ForEach-Object { $_.ToString('x2') }) -join ' '
}
function Record-Entries([string]$When) {
  $facts["run_$When"] = Run-Value
  $facts["approved_$When"] = Approved-Value
  Log "$When Run\Thalvi = $($facts["run_$When"]); StartupApproved\Run\Thalvi = $($facts["approved_$When"])"
}
function Add-Sentinel {
  Remove-Item $sentinel -ErrorAction SilentlyContinue
  Set-ItemProperty $runKey QaSentinel "C:\Windows\System32\conhost.exe --headless C:\Windows\System32\cmd.exe /c echo started>$sentinel"
  Log 'added the QaSentinel Run entry, which shows when the next logon has run its Run entries'
}

function Start-ByHand {
  $p = Start-Process $exe -PassThru -RedirectStandardOutput "$out\$Label-app.log" -RedirectStandardError "$out\$Label-app.err"
  Wait-Until { (Get-Content "$out\$Label-app.log" -Raw -ErrorAction SilentlyContinue) -match 'Transcribing through' } 'the app to finish starting' | Out-Null
  Log "started by hand: pid $($p.Id)"
  $p
}

$AE = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]::Descendants
function Settings-Window([int]$ProcessId) {
  [Win]::Of([uint32]$ProcessId) | Where-Object { [Win]::IsWindowVisible($_) -and [Win]::Title($_) -eq 'Thalvi settings' } | Select-Object -First 1
}
# A relaunch opens Settings in the running app; soon after start one can go unanswered, so it asks again.
function Settings-Element([int]$ProcessId) {
  $window = Wait-Until {
    Start-Process $exe -Wait
    $until = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $until -and -not (Settings-Window $ProcessId)) { Start-Sleep -Milliseconds 250 }
    Settings-Window $ProcessId
  } 'the Settings window' 45
  $AE::FromHandle($window)
}
function Named-Elements($Root, [string]$Pattern) {
  $all = $Root.FindAll($Scope, [System.Windows.Automation.Condition]::TrueCondition)
  @($all | Where-Object { $_.Current.Name -match $Pattern })
}
function Login-Switch($Settings) {
  $app = Wait-Until { Named-Elements $Settings '^App$' | Where-Object { $_.Current.ControlType -eq [System.Windows.Automation.ControlType]::Button } | Select-Object -First 1 } 'the App page button' 30
  $app.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
  Wait-Until {
    Named-Elements $Settings '^Start Thalvi when I log in$' | Where-Object { $_.GetSupportedPatterns() -contains [System.Windows.Automation.TogglePattern]::Pattern } | Select-Object -First 1
  } 'the login switch' 30
}
function Switch-State($Switch) { $Switch.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState.ToString() }
function Flip($Switch, [string]$Want) {
  $Switch.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Toggle()
  Wait-Until { (Switch-State $Switch) -eq $Want } "the switch to read $Want" 15 | Out-Null
  Log "flipped the login switch to $Want"
}
function Startup-Notes($Settings) {
  @(Named-Elements $Settings 'startup|log ?in|Start Thalvi' | ForEach-Object { $_.Current.Name } | Select-Object -Unique)
}

# Names on the taskbar and in the "Show hidden icons" flyout; Thalvi's tray icon is named "Thalvi: <state>".
function Tray-Names([string]$Shot) {
  $all = [System.Windows.Automation.Condition]::TrueCondition
  $children = [System.Windows.Automation.TreeScope]::Children
  $bar = $AE::RootElement.FindFirst($children, (New-Object System.Windows.Automation.PropertyCondition($AE::ClassNameProperty, 'Shell_TrayWnd')))
  if (-not $bar) { Log 'no taskbar found'; return @() }
  $names = @($bar.FindAll($Scope, $all) | ForEach-Object { $_.Current.Name } | Where-Object { $_ })
  $chevron = $bar.FindAll($Scope, $all) | Where-Object { $_.Current.Name -match 'hidden icons' } | Select-Object -First 1
  if ($chevron) {
    $patterns = $chevron.GetSupportedPatterns()
    if ($patterns -contains [System.Windows.Automation.InvokePattern]::Pattern) { $chevron.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke() }
    else { $chevron.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Toggle() }
    $deadline = (Get-Date).AddSeconds(5); $flyout = @()
    while ((Get-Date) -lt $deadline -and -not $flyout.Count) {
      Start-Sleep -Milliseconds 250
      $flyout = @($AE::RootElement.FindAll($children, $all) | Where-Object { $_.Current.ClassName -match 'Overflow' } | ForEach-Object { $_.FindAll($Scope, $all) } | ForEach-Object { $_.Current.Name } | Where-Object { $_ })
    }
    Save-Screen $Shot
    $names += $flyout
    [System.Windows.Forms.SendKeys]::SendWait('{ESC}')
  } else { Log 'no "Show hidden icons" button'; Save-Screen $Shot }
  Log "tray names: $(($names | Select-Object -Unique) -join ' | ')"
  @($names | Where-Object { $_ -match '^Thalvi' } | Select-Object -Unique)
}

function Observe-Started {
  # Every foreground change from this step's start, until 25 s after Thalvi first runs.
  $counts = @{}; $foregrounds = @{}; $timeline = New-Object System.Collections.Generic.List[string]
  $last = ''; $lastOverlay = ''; $first = $null; $deadline = (Get-Date).AddSeconds(120)
  while ((Get-Date) -lt $deadline) {
    $now = Get-Date -Format HH:mm:ss.fff
    $fg = Foreground
    if ($fg -ne $last) { $timeline.Add("$now foreground $fg"); $last = $fg }
    $foregrounds[$fg] = 1
    $running = Thalvi-Processes
    if ($running.Count -and -not $first) {
      $first = $running[0]
      $timeline.Add("$now Thalvi running, pid $($first.Id), started $($first.StartTime.ToString('HH:mm:ss.fff'))")
      $deadline = (Get-Date).AddSeconds(25)
    }
    if ($first) {
      $counts[$running.Count] = 1
      $overlay = (@(Windows-Of $first.Id) | Where-Object { $_ -match 'Thalvi overlay$' }) -join ''
      if ($overlay -ne $lastOverlay) { $timeline.Add("$now overlay $overlay"); $lastOverlay = $overlay }
    }
    Start-Sleep -Milliseconds 100
  }
  if (-not $first) { throw 'Thalvi did not start within 120 s of the step starting' }
  $facts.timeline = @($timeline)
  $timeline | ForEach-Object { Log "  $_" }
  $facts.instance_counts_seen = @($counts.Keys)
  $facts.foregrounds_seen = @($foregrounds.Keys)
  $facts.command_lines = Command-Lines
  $facts.windows = @(Windows-Of $first.Id)
  $facts.visible_windows = @(Visible-Titles)
  Log "instance counts seen over 25 s: $($facts.instance_counts_seen -join ', ')"
  Log "foreground windows seen: $($facts.foregrounds_seen -join ' | ')"
  Log "command lines: $($facts.command_lines -join ' | ')"
  Log "Thalvi's windows: $($facts.windows -join ' | ')"
  Log "visible windows: $($facts.visible_windows -join ' | ')"
  Save-Screen 'after-login'
  $facts.tray_icons = Tray-Names 'tray'
  Check ($facts.tray_icons.Count -eq 1) 'exactly one Thalvi tray icon is shown'
  Check (($counts.Keys | Measure-Object -Maximum).Maximum -eq 1 -and (Thalvi-Processes).Count -eq 1) 'exactly one Thalvi instance runs'
  Check (@($facts.command_lines | Where-Object { $_ -match '--login' }).Count -eq 1) 'it was started with --login'
  Check (-not ($facts.windows -contains 'visible Thalvi settings')) 'Settings is not open'
  Check (-not ($foregrounds.Keys | Where-Object { $_ -match '^thalvi-desktop#' })) 'Thalvi never held the foreground'
  $facts.foreground_at_end = Foreground
}

function Observe-Absent {
  $explorer = Get-Process explorer | Where-Object { $_.SessionId -eq $session } | Select-Object -First 1
  Wait-Until { Test-Path $sentinel } 'the QaSentinel Run entry to run' 120 | Out-Null
  Log "QaSentinel ran at $((Get-Item $sentinel).LastWriteTime.ToString('HH:mm:ss.fff')) (explorer started $($explorer.StartTime.ToString('HH:mm:ss.fff')))"
  $deadline = (Get-Date).AddSeconds(30)
  $ever = 0
  while ((Get-Date) -lt $deadline) { $ever = [Math]::Max($ever, (Thalvi-Processes).Count); Start-Sleep -Milliseconds 250 }
  $facts.visible_windows = @(Visible-Titles)
  Log "visible windows: $($facts.visible_windows -join ' | ')"
  Save-Screen 'after-login'
  Check ($ever -eq 0) 'no Thalvi started in the 30 s after the Run entries ran'
  Remove-ItemProperty $runKey QaSentinel
  Remove-Item $sentinel
  Record-Entries 'at_logon'
}

try {
  Log "step $Step in session $session as $env:USERDOMAIN\$env:USERNAME"
  switch ($Step) {
    'install' {
      $oldDir = Join-Path $env:LOCALAPPDATA 'Thalvi'
      $facts.old_dir_before = Test-Path $oldDir
      $setup = Start-Process 'C:\qa\thalvi-setup.exe' -ArgumentList '/S' -Wait -PassThru
      Log "silent install exited with $($setup.ExitCode)"
      Check (Test-Path $exe) "the silent install put the app at $exe"
      Check (-not (Test-Path $oldDir)) "the silent install created no $oldDir"
      $uninstallEntry = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Thalvi' -ErrorAction SilentlyContinue
      $facts.uninstall_entry = if ($uninstallEntry) { "$($uninstallEntry.DisplayName) $($uninstallEntry.DisplayVersion) at $($uninstallEntry.InstallLocation); $($uninstallEntry.UninstallString)" }
      $facts.start_menu = @(Get-ChildItem "$env:APPDATA\Microsoft\Windows\Start Menu\Programs" -Recurse -Filter 'Thalvi*.lnk' | ForEach-Object { $_.FullName })
      Log "uninstall entry: $($facts.uninstall_entry)"
      Log "Start menu: $($facts.start_menu -join ', ')"
      Check ($facts.start_menu.Count -ge 1) 'the install added a Start menu entry'
      $facts.desktop_shortcut = @(Get-ChildItem ([Environment]::GetFolderPath('Desktop')) -Filter 'Thalvi*.lnk' | ForEach-Object { $_.FullName })
      Log "desktop shortcut: $($facts.desktop_shortcut -join ', ')"

      $config = Join-Path $env:APPDATA 'thalvi\desktop.json'
      New-Item -ItemType Directory -Force (Split-Path $config) | Out-Null
      Set-Content $config '{"server":"ws://127.0.0.1:1","stt":{"engine":"server"}}'
      Record-Entries 'before'
      $app = Start-ByHand
      $settings = Settings-Element $app.Id
      $switch = Login-Switch $settings
      $facts.switch_initially = Switch-State $switch
      Check ($facts.switch_initially -eq 'Off') 'the login switch starts off'
      Flip $switch 'On'
      Save-Screen 'switch-on'
      $facts.tray_icons = Tray-Names 'tray'
      Check ($facts.tray_icons.Count -eq 1) 'a manual start shows exactly one Thalvi tray icon'
      Record-Entries 'after_on'
      Check ($facts.run_after_on -match 'Programs\\Thalvi\\thalvi-desktop\.exe"? --login$') 'the switch wrote a Run entry for the installed exe with --login'
      $facts.notes = Startup-Notes $settings
      Log "notes: $($facts.notes -join ' | ')"
      $facts.data_dir = @(Get-ChildItem (Join-Path $env:LOCALAPPDATA 'thalvi') -Recurse -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
    }
    'observe-started' { Observe-Started; Record-Entries 'at_logon' }
    'approved-off' {
      # What Task Manager writes when its Startup apps toggle is turned off: 03, then the time it was turned off.
      $bytes = [byte[]](@(3, 0, 0, 0) + [BitConverter]::GetBytes((Get-Date).ToFileTimeUtc()))
      New-Item -Force $approvedKey -ErrorAction SilentlyContinue | Out-Null
      Set-ItemProperty $approvedKey Thalvi $bytes -Type Binary
      Record-Entries 'after_toggle_off'
      Add-Sentinel
    }
    'observe-absent' { Observe-Absent }
    'manual-respects-approved' {
      $app = Start-ByHand
      Record-Entries 'after_manual_start'
      Check ($facts.run_after_manual_start -match '--login$' -and $facts.approved_after_manual_start -match '^03') 'a manual start left the Task Manager toggle off'
      $settings = Settings-Element $app.Id
      $switch = Login-Switch $settings
      $facts.switch_while_turned_off = Switch-State $switch
      $facts.notes = Startup-Notes $settings
      Log "switch reads $($facts.switch_while_turned_off); notes: $($facts.notes -join ' | ')"
      Save-Screen 'turned-off-in-task-manager'
      # The user then turns the switch off in Thalvi (on first when it reads off, so the off is Thalvi's own).
      if ($facts.switch_while_turned_off -eq 'Off') { Flip $switch 'On'; Record-Entries 'after_on_again' }
      Flip $switch 'Off'
      Save-Screen 'switch-off'
      Record-Entries 'after_off'
      Check ($null -eq $facts.run_after_off) 'switching off removed the Run entry'
      Stop-Process -Id $app.Id
      Add-Sentinel
    }
    'uninstall' {
      $app = Start-ByHand
      $settings = Settings-Element $app.Id
      $switch = Login-Switch $settings
      Flip $switch 'On'
      Record-Entries 'before_uninstall'
      Check ($null -ne $facts.run_before_uninstall) 'the switch is on again before uninstalling'
      $data = Join-Path $env:LOCALAPPDATA 'thalvi'
      New-Item -ItemType Directory -Force "$data\models", "$data\recordings" | Out-Null
      Set-Content "$data\models\qa-marker.bin" 'model'
      Set-Content "$data\recordings\qa-marker.txt" 'recording'
      $facts.data_before = @(Get-ChildItem $data -Recurse | ForEach-Object { $_.FullName.Substring($data.Length) })
      $facts.config_before = Test-Path (Join-Path $env:APPDATA 'thalvi\desktop.json')
      $uninstaller = Join-Path (Split-Path $exe) 'uninstall.exe'
      Log "uninstalling with $uninstaller /S while Thalvi runs (pid $($app.Id))"
      Start-Process $uninstaller -ArgumentList '/S' -Wait
      Wait-Until { -not (Test-Path $uninstaller) } 'the uninstaller to finish' 90 | Out-Null
      $facts.still_running = (Thalvi-Processes).Count
      $facts.program_dir_after = @(Get-ChildItem (Split-Path $exe) -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
      $facts.start_menu_after = @(Get-ChildItem "$env:APPDATA\Microsoft\Windows\Start Menu\Programs" -Recurse -Filter 'Thalvi*.lnk' | ForEach-Object { $_.FullName })
      $facts.desktop_shortcut_after = @(Get-ChildItem ([Environment]::GetFolderPath('Desktop')) -Filter 'Thalvi*.lnk' | ForEach-Object { $_.FullName })
      $facts.uninstall_entry_after = Test-Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Thalvi'
      $facts.data_after = @(Get-ChildItem $data -Recurse -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName.Substring($data.Length) })
      $facts.config_after = Test-Path (Join-Path $env:APPDATA 'thalvi\desktop.json')
      Record-Entries 'after_uninstall'
      Log "Thalvi processes after uninstall: $($facts.still_running); program folder now: $($facts.program_dir_after -join ', ')"
      Log "data before: $($facts.data_before -join ', ')"
      Log "data after:  $($facts.data_after -join ', ')"
      Check (-not (Test-Path $exe)) 'the uninstall removed the app'
      Check ($facts.start_menu_after.Count -eq 0) 'the uninstall removed the Start menu entry'
      Check ($facts.desktop_shortcut_after.Count -eq 0) 'the uninstall removed the desktop shortcut'
      Check (-not $facts.uninstall_entry_after) 'the uninstall removed its Apps entry'
      Check ($null -eq $facts.run_after_uninstall) 'the uninstall removed the Run entry'
      Check ($null -eq $facts.approved_after_uninstall) 'the uninstall removed the StartupApproved value'
      Check ((Compare-Object $facts.data_before $facts.data_after) -eq $null) "the uninstall kept every file in $data"
      Check ($facts.config_after) 'the uninstall kept desktop.json'
      Save-Screen 'after-uninstall'
      Add-Sentinel
    }
    default { throw "unknown step $Step" }
  }
  $result = if ($problems.Count) { "failed: $($problems -join '; ')" } else { 'ok' }
} catch {
  Log "ERROR $_ at $($_.InvocationInfo.PositionMessage)"
  Save-Screen 'error'
  $result = "error: $_"
} finally {
  $facts | ConvertTo-Json -Depth 4 | Out-File "$out\$Label.json" -Encoding utf8
  Set-Content "$out\$Label.done" $result
}
