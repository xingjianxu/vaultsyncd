# S167's maintained acceptance entry is S185: it retains the cold-start, reuse,
# single-writer, invalid-config and maintenance-barrier checks and adds movable folders.
param([switch]$Worker)
$ErrorActionPreference = 'Stop'
$acceptance = Join-Path $PSScriptRoot 's185-direct-startup.ps1'
if (!(Test-Path -LiteralPath $acceptance -PathType Leaf)) {
  throw 'S167 now delegates to S185; stage s185-direct-startup.ps1 alongside this compatibility entry'
}
& $acceptance @PSBoundParameters
