#Requires -Version 5.1
<#
.SYNOPSIS
  Thin CLI for fakeprint control plane, discovery assist, and Python lab harness.

.EXAMPLE
  .\scripts\fakeprint.ps1 status
  .\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('status', 'health', 'jobs', 'reset', 'lab', 'add', 'help')]
    [string]$Command = 'help',

    [string]$BaseUrl = 'http://127.0.0.1:3921',

    [Parameter(Position = 1)]
    [string]$Arg1,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Import-Module (Join-Path $here 'Fakeprint.psm1') -Force

function Show-Help {
    @'
fakeprint.ps1

  status [-BaseUrl URL]     GET /v1/status
  health [-BaseUrl URL]     GET /health
  jobs [-BaseUrl URL]       GET /v1/jobs
  reset [-BaseUrl URL]      POST /v1/reset (in-memory; inbox files kept)
  add                       Open Printers & scanners (person Add device)
  lab <fakeprint-lab args…> Python harness (pip install -e ./python[dev])

Examples:
  .\scripts\fakeprint.ps1 status
  .\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
'@ | Write-Host
}

switch ($Command) {
    'help' { Show-Help }
    'health' {
        $BaseUrl = (Resolve-FakeprintBaseUrl $BaseUrl)
        $r = Invoke-WebRequest -Uri "$BaseUrl/health" -UseBasicParsing -TimeoutSec 5
        [pscustomobject]@{ BaseUrl = $BaseUrl; Body = $r.Content.Trim(); Ok = ($r.Content.Trim() -eq 'ok') } |
            Format-List
    }
    'status' {
        $BaseUrl = (Resolve-FakeprintBaseUrl $BaseUrl)
        Invoke-RestMethod -Uri "$BaseUrl/v1/status" -TimeoutSec 5 | ConvertTo-Json -Depth 6
    }
    'jobs' {
        $BaseUrl = (Resolve-FakeprintBaseUrl $BaseUrl)
        Invoke-RestMethod -Uri "$BaseUrl/v1/jobs" -TimeoutSec 5 | ConvertTo-Json -Depth 6
    }
    'reset' {
        $BaseUrl = (Resolve-FakeprintBaseUrl $BaseUrl)
        Invoke-RestMethod -Method Post -Uri "$BaseUrl/v1/reset" -TimeoutSec 5 | ConvertTo-Json -Compress
    }
    'add' {
        Open-FakeprintAddDevice
    }
    'lab' {
        $pyArgs = @()
        if (-not [string]::IsNullOrWhiteSpace($Arg1)) { $pyArgs += $Arg1 }
        if ($Rest) { $pyArgs += $Rest }
        if ($pyArgs.Count -eq 0) {
            throw 'usage: lab <fakeprint-lab subcommand…>  e.g. lab print-soap python/fixtures/sample.pdf'
        }
        $repo = Split-Path -Parent $here
        $env:PYTHONPATH = (Join-Path $repo 'python')
        $probe = & python -c "import fakeprint, httpx; print('ok')" 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw @"
fakeprint Python package not importable (need httpx).
  cd $repo
  pip install -e `"./python[dev]`"
Probe: $probe
"@
        }
        & python -m fakeprint.lab --base-url $BaseUrl @pyArgs
        if ($LASTEXITCODE -ne 0) {
            throw "fakeprint-lab exited with code $LASTEXITCODE"
        }
    }
}
