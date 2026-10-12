#Requires -Version 5.1
<#
.SYNOPSIS
  Ensure narrowly scoped inbound allows for fakeprint TCP 3921 and JetDirect 9100 (Private).

.DESCRIPTION
  Creates (or reports) firewall rules:
    - Fakeprint-WSD-3921  — WSD SOAP / control / IPP HTTP
    - Fakeprint-JD-9100   — TCPMON RAW JetDirect (lab fall-back queue)

  Does not disable the firewall, does not touch fakescan rules, and does not
  open UDP 3702 by name (Windows Network Discovery / existing fakeprint Query
  User UDP rules usually cover WS-Discovery Hellos on this lab).

.EXAMPLE
  # Elevated PowerShell:
  .\scripts\ensure-firewall.ps1

.EXAMPLE
  # Preview only:
  .\scripts\ensure-firewall.ps1 -WhatIf
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

function Test-IsAdministrator {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = [Security.Principal.WindowsPrincipal]::new($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Ensure-FakeprintFirewallRule {
    param(
        [Parameter(Mandatory)][string]$RuleName,
        [Parameter(Mandatory)][string]$DisplayName,
        [Parameter(Mandatory)][string]$Description,
        [Parameter(Mandatory)][uint16]$LocalPort
    )

    $existing = Get-NetFirewallRule -Name $RuleName -ErrorAction SilentlyContinue
    if (-not $existing) {
        $existing = Get-NetFirewallRule -DisplayName $DisplayName -ErrorAction SilentlyContinue
    }

    if ($existing -and -not $Force) {
        $ports = Get-NetFirewallPortFilter -AssociatedNetFirewallRule $existing
        Write-Host "OK: rule already present."
        Write-Host ("  Name={0} Enabled={1} Direction={2} Action={3} Profile={4} Protocol={5} LocalPort={6}" -f `
            $existing.Name, $existing.Enabled, $existing.Direction, $existing.Action, $existing.Profile, $ports.Protocol, ($ports.LocalPort -join ','))
        return
    }

    if (-not (Test-IsAdministrator)) {
        Write-Warning "Not elevated - cannot create rule $DisplayName from this shell."
        Write-Host @"
New-NetFirewallRule ``
  -Name '$RuleName' ``
  -DisplayName '$DisplayName' ``
  -Description '$Description' ``
  -Direction Inbound ``
  -Action Allow ``
  -Protocol TCP ``
  -LocalPort $LocalPort ``
  -Profile Private
"@
        return
    }

    if ($existing -and $Force) {
        if ($PSCmdlet.ShouldProcess($DisplayName, 'Remove existing firewall rule')) {
            Remove-NetFirewallRule -Name $existing.Name -ErrorAction SilentlyContinue
            Remove-NetFirewallRule -DisplayName $DisplayName -ErrorAction SilentlyContinue
        }
    }

    if ($PSCmdlet.ShouldProcess($DisplayName, "Create inbound TCP $LocalPort Private allow")) {
        New-NetFirewallRule `
            -Name $RuleName `
            -DisplayName $DisplayName `
            -Description $Description `
            -Direction Inbound `
            -Action Allow `
            -Protocol TCP `
            -LocalPort $LocalPort `
            -Profile Private | Out-Null
        Write-Host "Created firewall rule: $DisplayName (Inbound TCP $LocalPort, Private)."
    }

    Get-NetFirewallRule -DisplayName $DisplayName | Format-List Name, DisplayName, Enabled, Direction, Action, Profile
}

Ensure-FakeprintFirewallRule `
    -RuleName 'Fakeprint-WSD-3921' `
    -DisplayName 'Fakeprint-WSD-3921' `
    -Description 'Lab: inbound TCP 3921 for fakeprint WSD/IPP HTTP (Private profile only).' `
    -LocalPort 3921

Ensure-FakeprintFirewallRule `
    -RuleName 'Fakeprint-JD-9100' `
    -DisplayName 'Fakeprint-JD-9100' `
    -Description 'Lab: inbound TCP 9100 JetDirect/AppSocket for TCPMON RAW fall-back (Private profile only).' `
    -LocalPort 9100

if (-not (Test-IsAdministrator)) {
    exit 2
}
