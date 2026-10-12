#Requires -Version 5.1
<#
.SYNOPSIS
  Enumerate whether Windows FD / WSD / Add list can see Fake WSD printer.
.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File D:\code\fakeprint\scripts\probe-discovery.ps1
  powershell -NoProfile -ExecutionPolicy Bypass -File D:\code\fakeprint\scripts\probe-discovery.ps1 -OpenAddUi
#>
[CmdletBinding()]
param(
    [switch]$OpenAddUi,
    [int]$FdTimeoutMs = 10000,
    [int]$AepTimeoutMs = 25000
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'Fakeprint.psm1') -Force

$r = Get-FakeprintDiscoveryReport -FdTimeoutMs $FdTimeoutMs -AepTimeoutMs $AepTimeoutMs -OpenAddUi:$OpenAddUi

Write-Host '=== /v1/status ==='
if ($r.Status) {
    $r.Status | Select-Object label, device_uuid, device_endpoint, wsd_hellos_sent, wsd_probes_answered |
        Format-List
} else {
    Write-Warning $r.StatusError
}

Write-Host '=== Transfer Get ==='
if ($r.TransferGet) { $r.TransferGet | Format-List } else { Write-Warning $r.TransferGetError }

Write-Host '=== Get-Printer (fakeprint-like) ==='
if ($r.InstalledPrinters.Count) { $r.InstalledPrinters | Format-Table -AutoSize }
else { Write-Host '(none)' }

Write-Host '=== PnP fakeprint-like ==='
if ($r.PnpFakeprint.Count) { $r.PnpFakeprint | Format-Table -AutoSize -Wrap }
else { Write-Host '(none)' }

Write-Host '=== Function Discovery queries ==='
$r.FunctionDiscovery.Queries | Format-Table -AutoSize
Write-Host ("FakeprintPresent={0} Installable={1} hitCount={2}" -f `
    $r.FunctionDiscovery.FakeprintPresent, $r.FunctionDiscovery.InstallableFakeprint, `
    @($r.FunctionDiscovery.FakeprintHits).Count)
if ($r.FunctionDiscovery.FakeprintHits.Count) {
    $r.FunctionDiscovery.FakeprintHits |
        Select-Object FriendlyName, Installable, DeviceCategory, XAddrs, GlobalIdentity, Types |
        Format-List
} else {
    Write-Host 'All FD hits (name / category / xaddrs) for context:'
    @($r.FunctionDiscovery.Hits) |
        Select-Object FriendlyName, DeviceCategory, Installable, XAddrs, IpAddress |
        Format-Table -AutoSize -Wrap
}

Write-Host '=== FD with PrintDeviceType constraint ==='
Write-Host ("FakeprintPresent={0} hits={1}" -f `
    $r.FunctionDiscoveryPrintType.FakeprintPresent, `
    @($r.FunctionDiscoveryPrintType.FakeprintHits).Count)
@($r.FunctionDiscoveryPrintType.Hits) |
    Select-Object FriendlyName, DeviceCategory, Installable, XAddrs |
    Format-Table -AutoSize -Wrap

Write-Host '=== WinRT AEP (WSD) ==='
Write-Host ("Ok={0} FakeprintPresent={1} CanPair={2} total={3}" -f `
    $r.AssociationEndpoints.Ok, $r.AssociationEndpoints.FakeprintPresent, `
    $r.AssociationEndpoints.CanPairFakeprint, @($r.AssociationEndpoints.Hits).Count)
if ($r.AssociationEndpoints.Error) { Write-Warning $r.AssociationEndpoints.Error }
if ($r.AssociationEndpoints.FakeprintHits.Count) {
    $r.AssociationEndpoints.FakeprintHits | Format-Table -AutoSize -Wrap
} else {
    Write-Host 'Other AEP names:'
    @($r.AssociationEndpoints.Hits) | Select-Object Name, IsPaired, CanPair | Format-Table -AutoSize
}

Write-Host '=== Verdict ==='
[pscustomobject]@{
    VisibleInFd = $r.VisibleInFd
    InstallableInFd = $r.InstallableInFd
    VisibleInAep = $r.VisibleInAep
    CanPairInAep = $r.CanPairInAep
    InstalledQueue = $r.InstalledQueue
    InstalledPnp = $r.InstalledPnp
    TransferGetOk = [bool]($r.TransferGet -and $r.TransferGet.Ok)
} | Format-List

# Person glance: Add list is Settings UI only.
Write-Host 'Add list: open ms-settings:printers and look for Fake WSD printer under Add device (person glance).'
