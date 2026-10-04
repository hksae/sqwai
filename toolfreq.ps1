param([string]$JournalDir = ".\.sqwai\journal")
# Counts tool_call records in .sqwai/journal/*.jsonl by tool name.
# Usage: powershell -File toolfreq.ps1 [-JournalDir <path>]
$files = Get-ChildItem -Path $JournalDir -Filter *.jsonl -ErrorAction SilentlyContinue
if (-not $files) { Write-Output ("no journals in " + $JournalDir); exit 1 }
$counts = @{}
$total = 0
$lines = 0
foreach ($f in $files) {
  foreach ($line in [System.IO.File]::ReadLines($f.FullName)) {
    $lines++
    if ($line -notmatch '"tool_call"') { continue }
    try { $rec = $line | ConvertFrom-Json -ErrorAction Stop } catch { continue }
    if ($rec.kind -ne 'tool_call') { continue }
    $name = [string]$rec.tool
    if ([string]::IsNullOrEmpty($name)) { continue }
    if (-not $counts.ContainsKey($name)) { $counts[$name] = 0 }
    $counts[$name]++
    $total++
  }
}
$protocol = @('plan', 'ask_user', 'propose_plan', 'propose_reset', 'subagent', 'think')
Write-Output ('files: ' + $files.Count + ', lines: ' + $lines + ', tool_calls: ' + $total)
Write-Output ''
Write-Output 'name                      calls      share  flag'
foreach ($k in ($counts.Keys | Sort-Object { -$counts[$_] })) {
  $pct = 100.0 * $counts[$k] / [Math]::Max(1, $total)
  $flag = ''
  if ($pct -lt 1.0) {
    if ($protocol -contains $k) { $flag = 'rare+protocol' } else { $flag = 'RARE?' }
  }
  Write-Output ($k.PadRight(24) + ' ' + ([string]$counts[$k]).PadLeft(7) + ' ' + $pct.ToString('0.0').PadLeft(6) + '%  ' + $flag)
}
