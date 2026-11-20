# The dictation comparison ignores case and punctuation and keeps Notepad's
# actual text in a mismatch. It does not need Windows.
$ErrorActionPreference = 'Stop'
$path = Join-Path $PSScriptRoot 'transcript.ps1'
if (-not (Test-Path -LiteralPath $path)) {
  throw 'transcript assertion is absent: .github/scripts/e2e/windows/transcript.ps1'
}
. $path

$failed = New-Object System.Collections.Generic.List[string]

function Check([string]$Name, [scriptblock]$Body) {
  try {
    & $Body
    Write-Output "PASS $Name"
  } catch {
    Write-Output "FAIL $Name :: $($_.Exception.Message)"
    $script:failed.Add($Name)
  }
}

function Assert-Matches([string]$Actual, [string]$Expected) {
  Assert-DictationText -Actual $Actual -Expected $Expected
}

function Assert-Rejects([string]$Actual, [string]$Expected) {
  $threw = $false
  $message = ''
  try {
    Assert-DictationText -Actual $Actual -Expected $Expected
  } catch {
    $threw = $true
    $message = $_.Exception.Message
  }
  if (-not $threw) { throw "accepted '$Actual' against '$Expected'" }
  if ($message -notlike "*'$Actual'*") {
    throw "mismatch hid Notepad's text. message: $message"
  }
}

Check 'case and punctuation differences match' {
  Assert-Matches 'Hello, world!' 'hello world'
  Assert-Matches 'HELLO WORLD' 'hello world'
  Assert-Matches "It's fine." 'its fine'
  Assert-Matches 'hello   world' 'hello world'
  Assert-Matches 'room 12.' 'room 12'
}

Check 'a different phrase fails with the actual text' {
  Assert-Rejects 'hello world' 'hello word'
  Assert-Rejects 'hello there world' 'hello world'
  Assert-Rejects '' 'hello world'
  Assert-Rejects "don't stop" 'do not stop'
}

if ($failed.Count -gt 0) { exit 1 }
