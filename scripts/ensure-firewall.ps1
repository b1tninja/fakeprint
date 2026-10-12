#Requires -Version 5.1
<#
.SYNOPSIS
  Ensure a narrowly scoped inbound allow for fakeprint TCP 3921 (Private).

.DESCRIPTION
  Creates (or reports) firewall rule DisplayName/Name: Fakeprint-WSD-3921
  Inbound TCP LocalPort 3921, Profile Private only.

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
$RuleName = 'Fakeprint-WSD-3921'
$DisplayName = 'Fakeprint-WSD-3921'
$Description = 'Lab: inbound TCP 3921 for fakeprint WSD print SOAP/control (Private profile only).'

function Test-IsAdministrator {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = [Security.Principal.WindowsPrincipal]::new($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

$existing = Get-NetFirewallRule -Name $RuleName -ErrorAction SilentlyContinue
if (-not $existing) {
    $existing = Get-NetFirewallRule -DisplayName $DisplayName -ErrorAction SilentlyContinue
}

if ($existing -and -not $Force) {
    $ports = Get-NetFirewallPortFilter -AssociatedNetFirewallRule $existing
    Write-Host "OK: rule already present."
    Write-Host ("  Name={0} Enabled={1} Direction={2} Action={3} Profile={4} Protocol={5} LocalPort={6}" -f `
        $existing.Name, $existing.Enabled, $existing.Direction, $existing.Action, $existing.Profile, $ports.Protocol, ($ports.LocalPort -join ','))
    exit 0
}

$newRule = @'
New-NetFirewallRule `
  -Name 'Fakeprint-WSD-3921' `
  -DisplayName 'Fakeprint-WSD-3921' `
  -Description 'Lab: inbound TCP 3921 for fakeprint WSD print SOAP/control (Private profile only).' `
  -Direction Inbound `
  -Action Allow `
  -Protocol TCP `
  -LocalPort 3921 `
  -Profile Private
'@

if (-not (Test-IsAdministrator)) {
    Write-Warning 'Not elevated - cannot create the rule from this shell.'    Write-Host ''
    Write-Host 'Run this in an elevated PowerShell (or re-run this script elevated):'
    Write-Host $newRule
    Write-Host ''
    Write-Host 'Verify:'
    Write-Host "  Get-NetFirewallRule -DisplayName '$DisplayName' | Format-List"
    Write-Host '  Test-NetConnection -ComputerName 192.168.1.70 -Port 3921'
    exit 2
}

if ($existing -and $Force) {
    if ($PSCmdlet.ShouldProcess($DisplayName, 'Remove existing firewall rule')) {
        Remove-NetFirewallRule -Name $existing.Name -ErrorAction SilentlyContinue
        Remove-NetFirewallRule -DisplayName $DisplayName -ErrorAction SilentlyContinue
    }
}

if ($PSCmdlet.ShouldProcess($DisplayName, 'Create inbound TCP 3921 Private allow')) {
    New-NetFirewallRule `
        -Name $RuleName `
        -DisplayName $DisplayName `
        -Description $Description `
        -Direction Inbound `
        -Action Allow `
        -Protocol TCP `
        -LocalPort 3921 `
        -Profile Private | Out-Null
    Write-Host "Created firewall rule: $DisplayName (Inbound TCP 3921, Private)."
}

Get-NetFirewallRule -DisplayName $DisplayName | Format-List Name, DisplayName, Enabled, Direction, Action, Profile
