# Reads the focused Notepad editor. Windows PowerShell 5.1: UI Automation is a .NET Framework assembly.
param([Parameter(Mandatory)][int]$ProcessId, [switch]$PrepareFocus)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
$OutputEncoding = [Console]::OutputEncoding

function Write-Result([bool]$Ok, [string]$Text, [string]$Error) {
  [pscustomobject]@{
    ok = $Ok
    text = $Text
    error = $Error
    process = 'notepad'
  } | ConvertTo-Json -Compress
}

function Test-Editor($Element) {
  $name = $Element.Current.ControlType.ProgrammaticName
  return $name -eq 'ControlType.Edit' -or $name -eq 'ControlType.Document'
}

function Find-Editor($Start) {
  $node = $Start
  while ($null -ne $node) {
    if ($node.Current.ProcessId -ne $ProcessId) { return $null }
    if (Test-Editor $node) { return $node }
    $node = [System.Windows.Automation.TreeWalker]::ControlViewWalker.GetParent($node)
  }
  return $null
}

function Get-EditorText($Element) {
  try {
    $pattern = $Element.GetCurrentPattern([System.Windows.Automation.TextPattern]::Pattern)
    return $pattern.DocumentRange.GetText(-1)
  } catch {}
  try {
    $pattern = $Element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
    return $pattern.Current.Value
  } catch {}
  throw 'Notepad editor exposed neither TextPattern nor ValuePattern'
}

try {
  Add-Type -AssemblyName UIAutomationClient
  Add-Type -AssemblyName UIAutomationTypes
  Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SmokeFocus {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
  public static uint ForegroundProcess() {
    uint process;
    GetWindowThreadProcessId(GetForegroundWindow(), out process);
    return process;
  }
  public static IntPtr ForegroundWindow() {
    return GetForegroundWindow();
  }
}
'@
  $process = Get-Process -Id $ProcessId
  if ($process.ProcessName -ne 'notepad') {
    throw "process is $($process.ProcessName), not notepad"
  }
  $foreground = [SmokeFocus]::ForegroundProcess()
  if ([int]$foreground -ne $ProcessId) {
    throw "foreground process is $foreground, not $ProcessId"
  }
  $focused = [System.Windows.Automation.AutomationElement]::FocusedElement
  $editor = Find-Editor $focused
  if ($PrepareFocus -and $null -eq $editor) {
    $root = [System.Windows.Automation.AutomationElement]::FromHandle([SmokeFocus]::ForegroundWindow())
    $edit = New-Object System.Windows.Automation.PropertyCondition(
      [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
      [System.Windows.Automation.ControlType]::Edit)
    $document = New-Object System.Windows.Automation.PropertyCondition(
      [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
      [System.Windows.Automation.ControlType]::Document)
    $either = New-Object System.Windows.Automation.OrCondition($edit, $document)
    $editor = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $either)
    if ($null -eq $editor) { throw 'Notepad has no editor' }
    $editor.SetFocus()
    $focused = [System.Windows.Automation.AutomationElement]::FocusedElement
    $editor = Find-Editor $focused
    if ($null -eq $editor) { throw "Notepad's editor does not have focus" }
  }
  if ($null -eq $editor) { throw "Notepad's editor does not have focus" }
  Write-Result $true ((Get-EditorText $editor) -replace "`0", '') ''
  exit 0
} catch {
  Write-Result $false '' $_.Exception.Message
  exit 1
}
