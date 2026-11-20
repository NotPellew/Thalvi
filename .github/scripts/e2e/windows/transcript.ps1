# Compares delivered dictation with an expected transcript, ignoring case and punctuation.

function ConvertTo-DictationComparable {
  param([Parameter(Mandatory)][AllowEmptyString()][string]$Text)
  $stripped = [regex]::Replace($Text.ToLowerInvariant(), '\p{P}+', '')
  return [regex]::Replace($stripped, '\s+', ' ').Trim()
}

function Assert-DictationText {
  param(
    [Parameter(Mandatory)][AllowEmptyString()][string]$Actual,
    [Parameter(Mandatory)][AllowEmptyString()][string]$Expected
  )
  $left = ConvertTo-DictationComparable -Text $Actual
  $right = ConvertTo-DictationComparable -Text $Expected
  if ($left -ne $right) {
    throw "Notepad text '$Actual' does not match the expected transcript '$Expected' ignoring case and punctuation."
  }
}
