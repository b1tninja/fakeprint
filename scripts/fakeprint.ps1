#Requires -Version 5.1
<#
.SYNOPSIS
  Thin CLI for fakeprint: control plane, device registry, discovery assist, and Python lab harness.

.DESCRIPTION
  fakeprint uses ODD ports (3921, 3923, ...). fakescan uses even ports. These scripts
  never touch fakescan processes or %LOCALAPPDATA%\fakescan.

.EXAMPLE
  .\scripts\fakeprint.ps1 status
  .\scripts\fakeprint.ps1 ensure
  .\scripts\fakeprint.ps1 ensure -Name bpl0
  .\scripts\fakeprint.ps1 ctl status
  .\scripts\fakeprint.ps1 ctl profile -CtlArgs @{id='generic-wsd'}
  .\scripts\fakeprint.ps1 list
  .\scripts\fakeprint.ps1 add -Name bpl0
  .\scripts\fakeprint.ps1 remove -Name bpl0
  .\scripts\fakeprint.ps1 save
  .\scripts\fakeprint.ps1 mark-added
  .\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
  .\scripts\fakeprint.ps1 print-test
  .\scripts\fakeprint.ps1 lab print-test
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet(
        'status', 'health', 'jobs', 'reset',
        'profile',
        'ctl',
        'ensure',
        'list', 'add', 'remove', 'save', 'mark-added',
        'install-ipp', 'remove-ipp', 'elevated',
        'add-device', 'watch-wsd', 'lab',
        'print-test', 'open-job',
        'help'
    )]
    [string]$Command = 'help',

    [string]$BaseUrl = 'http://127.0.0.1:3921',

    # Instance name for ensure/add/remove/save/mark-added/ctl
    [string]$Name = 'default',

    # Ctl command (for 'ctl' subcommand)
    [Parameter(Position = 1)]
    [string]$Arg1,

    # Extra args for ctl or lab
    [hashtable]$CtlArgs = @{},

    # Attention flags (for ensure / add-device):
    #   -Loud    FlashWindowEx on parent terminal window (opt-in).
    #   -Quiet   Banner only -- suppresses beep and balloon.
    #   Default  Banner + beep + BEL + NotifyIcon balloon.
    [switch]$Loud,
    [switch]$Quiet,

    # ensure / install-ipp: skip IPP queue install; open Settings -> Printers &
    # scanners for the person-click WSD path instead.
    [switch]$WsdAdd,

    # install-ipp: printer display name (default: profile label from /v1/status).
    [string]$PrinterName,

    # install-ipp: force re-create port + queue even if they already exist.
    [switch]$Force,

    # print-test: open the capture as a .pdf after PrintTo.
    [switch]$Open,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Import-Module (Join-Path $here 'Fakeprint.psm1') -Force

function Show-Help {
    @'
fakeprint.ps1  -- fakeprint control plane + device registry

Control plane (needs fakeprint running):
  status [-BaseUrl URL]          GET /v1/status
  health [-BaseUrl URL]          GET /health
  jobs [-BaseUrl URL]            GET /v1/jobs
  reset [-BaseUrl URL]           POST /v1/reset (in-memory; inbox files kept)
  profile <id> [-BaseUrl URL]   POST /v1/profile  (hot-swap profile)
  ctl <cmd> [-CtlArgs @{...}]     POST /v1/ctl  (cmd: status health jobs reset profile save)

Registry (%LOCALAPPDATA%\fakeprint\devices.json):
  list                           Show all device rows
  add [-Name NAME]               Add a named device row (assigns next odd port)
  remove -Name NAME              Remove a device row (does not stop the process)
  save [-Name NAME] [-BaseUrl]   Persist running process config into registry row
  mark-added [-Name NAME]        Set added.pnp=true (after printer is installed)

Process lifecycle:
  ensure [-Name NAME]            Attach if running, start if not. When added.pnp=false
                                 and running as admin, installs IPP queue automatically.
  ensure -Name all (use -All via module) -- start all autostart rows
  ensure -WsdAdd                 Skip IPP; open Settings -> Printers & scanners instead

IPP spooler queue (non-interactive; admin / UAC):
  install-ipp [-Name NAME]       Add-PrinterPort + Add-Printer with Microsoft IPP
                                 Class Driver. Marks added.pnp=true on success.
                                 Options: -PrinterName NAME  -Force  -WsdAdd
                                 Non-admin: launches elevated one-shot (UAC).
  remove-ipp  [-Name NAME]       Remove the IPP port + queue; set added.pnp=false.
  elevated                       Open elevated PowerShell with Fakeprint.psm1 loaded

Python lab harness:
  lab <fakeprint-lab args...>      Requires: pip install -e ./python[dev]
  lab print-test                   SOAP print visible testpage.pdf + magic assert
  lab prove-capture [file]         Score inbox .bin (magic / signature RGB fills)
  lab print-soap fixtures/sample.pdf
                                   Blank minimal PDF (byte-exact CI); prefer print-test

Spooler proof (visible page, not blank):
  print-test [-PrinterName NAME] [-Open]
                                   PrintTo fixtures/testpage.pdf; score capture
                                   (magic and/or signature RGB fills). -Open
                                   copies the .bin to a temp .pdf and opens it.
  open-job [-JobId N]              Preview latest (or N) inbox capture as PDF.

Windows Add-device (person-click WSD path):
  add-device                     Open Settings -> Printers & scanners, then poll
                                 for Subscribe/GetPrinterElements/PRINTENUM/WSD
                                 queue milestones (Watch-FakeprintWsdAdd).
  watch-wsd                      Poll /v1/status for WSD Add milestones only
                                 (run in a second window; does not open Settings).

Examples:
  .\scripts\fakeprint.ps1 ensure
  .\scripts\fakeprint.ps1 ensure -WsdAdd                        # WSD path instead
  .\scripts\fakeprint.ps1 install-ipp                           # admin; explicit IPP install
  .\scripts\fakeprint.ps1 install-ipp -Force                    # re-create queue
  .\scripts\fakeprint.ps1 remove-ipp                            # remove queue + port
  .\scripts\fakeprint.ps1 print-test                            # visible Letter page
  .\scripts\fakeprint.ps1 ctl status
  .\scripts\fakeprint.ps1 ctl profile -CtlArgs @{id='generic-wsd'}
  .\scripts\fakeprint.ps1 list
  .\scripts\fakeprint.ps1 add -Name bpl0
  .\scripts\fakeprint.ps1 lab print-test
  .\scripts\fakeprint.ps1 lab print-soap python/fixtures/sample.pdf
'@ | Write-Host
}

switch ($Command) {
    'help' { Show-Help }

    'health' {
        $u = (Resolve-FakeprintBaseUrl $BaseUrl)
        $r = Invoke-WebRequest -Uri "$u/health" -UseBasicParsing -TimeoutSec 5
        [pscustomobject]@{ BaseUrl = $u; Body = $r.Content.Trim(); Ok = ($r.Content.Trim() -eq 'ok') } |
            Format-List
    }

    'status' {
        $u = (Resolve-FakeprintBaseUrl $BaseUrl)
        Invoke-RestMethod -Uri "$u/v1/status" -TimeoutSec 5 | ConvertTo-Json -Depth 6
    }

    'jobs' {
        $u = (Resolve-FakeprintBaseUrl $BaseUrl)
        Invoke-RestMethod -Uri "$u/v1/jobs" -TimeoutSec 5 | ConvertTo-Json -Depth 6
    }

    'reset' {
        $u = (Resolve-FakeprintBaseUrl $BaseUrl)
        Invoke-RestMethod -Method Post -Uri "$u/v1/reset" -TimeoutSec 5 | ConvertTo-Json -Compress
    }

    'profile' {
        if ([string]::IsNullOrWhiteSpace($Arg1)) { throw 'usage: profile <id>' }
        $u = (Resolve-FakeprintBaseUrl $BaseUrl)
        $body = @{ id = $Arg1 } | ConvertTo-Json -Compress
        Invoke-RestMethod -Method Post -Uri "$u/v1/profile" `
            -ContentType 'application/json' -Body $body -TimeoutSec 5 | ConvertTo-Json -Compress
    }

    'ctl' {
        $cmd = if ([string]::IsNullOrWhiteSpace($Arg1)) { 'status' } else { $Arg1 }
        Invoke-FakeprintCtl -Cmd $cmd -Args $CtlArgs -BaseUrl $BaseUrl | ConvertTo-Json -Depth 6
    }

    'ensure' {
        $repo = Split-Path -Parent $here
        Invoke-FakeprintEnsure -Name $Name -RepoRoot $repo -WsdAdd:$WsdAdd -Loud:$Loud -Quiet:$Quiet
    }

    'list' {
        Write-Host "fakeprint registry: $(Get-FakeprintRegistryPath)"
        Get-FakeprintDevices | ConvertTo-Json -Depth 4
    }

    'add' {
        $adv = if ($Rest -and $Rest.Count -gt 0) { $Rest[0] } else { '' }
        $row = Add-FakeprintDevice -Name $Name -Advertise $adv
        Write-Host "Added device '$Name' to fakeprint registry:"
        $row | ConvertTo-Json -Depth 4
    }

    'remove' {
        if ($Name -eq 'default') {
            Write-Warning "Removing 'default' row. Are you sure? Pass -Name default explicitly if intended."
        }
        Remove-FakeprintDevice -Name $Name
    }

    'save' {
        Save-FakeprintDevice -Name $Name -BaseUrl $BaseUrl
    }

    'mark-added' {
        Set-FakeprintDeviceAdded -Name $Name -Pnp $true
    }

    'install-ipp' {
        if ($WsdAdd) {
            Install-FakeprintIppQueue -Name $Name -BaseUrl $BaseUrl -WsdAdd
        } elseif (Test-FakeprintIsAdmin) {
            $params = @{ Name = $Name; BaseUrl = $BaseUrl; Force = $Force }
            if (-not [string]::IsNullOrWhiteSpace($PrinterName)) { $params['PrinterName'] = $PrinterName }
            Install-FakeprintIppQueue @params
        } else {
            $params = @{ Name = $Name; BaseUrl = $BaseUrl; Force = $Force }
            if (-not [string]::IsNullOrWhiteSpace($PrinterName)) { $params['PrinterName'] = $PrinterName }
            Start-FakeprintElevatedInstallIpp @params
        }
    }

    'remove-ipp' {
        $params = @{ Name = $Name; BaseUrl = $BaseUrl }
        if (-not [string]::IsNullOrWhiteSpace($PrinterName)) { $params['PrinterName'] = $PrinterName }
        Remove-FakeprintIppQueue @params
    }

    'elevated' {
        Start-FakeprintElevatedShell
    }

    'add-device' {
        Open-FakeprintAddDevice -Loud:$Loud -Quiet:$Quiet
        # Immediately start the milestone watcher — person clicks Add device
        # while this loop runs. Press Ctrl+C to abort early.
        Watch-FakeprintWsdAdd -BaseUrl $BaseUrl
    }

    'watch-wsd' {
        # Standalone watcher: run in a second window while Settings is open.
        Watch-FakeprintWsdAdd -BaseUrl $BaseUrl
    }

    'print-test' {
        $printer = $PrinterName
        if ([string]::IsNullOrWhiteSpace($printer) -and -not [string]::IsNullOrWhiteSpace($Arg1)) {
            $printer = $Arg1
        }
        $r = Send-FakeprintTestPage -BaseUrl $BaseUrl -PrinterName $printer -Rebuild -Open:$Open
        $r | Format-List
        if (-not $r.Ok) { throw "print-test failed: $($r.Error)$($r.Note)" }
    }

    'open-job' {
        $jid = 0
        if (-not [string]::IsNullOrWhiteSpace($Arg1)) {
            $jid = [int]$Arg1
        }
        Open-FakeprintCapture -BaseUrl $BaseUrl -JobId $jid | Format-List
    }

    'lab' {
        $pyArgs = @()
        if (-not [string]::IsNullOrWhiteSpace($Arg1)) { $pyArgs += $Arg1 }
        if ($Rest) { $pyArgs += $Rest }
        if ($pyArgs.Count -eq 0) {
            throw 'usage: lab <fakeprint-lab subcommand...>  e.g. lab print-soap python/fixtures/sample.pdf'
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
