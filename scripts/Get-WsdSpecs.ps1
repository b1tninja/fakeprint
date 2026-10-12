#Requires -Version 5.1
<#
.SYNOPSIS
  Download the official WS-Print and WS-Scan specification packages.

.DESCRIPTION
  Packages from the file-downloads section of
  https://learn.microsoft.com/en-us/windows-hardware/design/whitepapers/implementing-web-services-on-devices-for-printing#file-downloads

  Print Service Definition v2.0
    https://go.microsoft.com/fwlink/p/?LinkId=534008

  WS-Print v1.2 (Printer.zip, 2013-09-16, 2.64 MB)
    https://download.microsoft.com/download/E/9/7/E974CFCB-4B3B-40CC-AF92-4F7F84477F0B/Printer.zip

  Print Device Definition v1.0
    https://download.microsoft.com/download/9/c/5/9c5b2167-8017-4bae-9fde-d599bac8184a/PrintDevice.zip

  Scan Service Definition v1.0
    https://download.microsoft.com/download/9/C/5/9C5B2167-8017-4BAE-9FDE-D599BAC8184A/ScanService.zip

  Scan Device Definition v1.0
    https://download.microsoft.com/download/9/c/5/9c5b2167-8017-4bae-9fde-d599bac8184a/ScanDevice.zip

  Each zip is saved under docs/reference/upstream/ and is not committed.
  *.xsd and *.wsdl are extracted for the selected service into docs/reference/schema/.
  Word documents stay inside the zip. WS-Print v2.0 is extracted under
  docs/reference/upstream/print-v2/ so it does not replace the v1.2 schemas.

.PARAMETER Service
  Print, Scan, or All. Default is Print in fakeprint, Scan in fakescan, otherwise All.

.PARAMETER Force
  Download again even when the zip is already on disk.

.PARAMETER AcceptLicense
  Record acceptance without a prompt. Use this only when you accept the agreement.
  A terminal run asks you to type ACCEPT instead.

.EXAMPLE
  .\scripts\Get-WsdSpecs.ps1

.EXAMPLE
  .\scripts\Get-WsdSpecs.ps1 -Service All -Force
#>
[CmdletBinding()]
param(
    [ValidateSet('Print', 'Scan', 'All')]
    [string]$Service,
    [switch]$Force,
    [switch]$AcceptLicense
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$RepoRoot = Split-Path $PSScriptRoot -Parent
$Reference = Join-Path (Join-Path $RepoRoot 'docs') 'reference'
$Upstream = Join-Path $Reference 'upstream'

if (-not $PSBoundParameters.ContainsKey('Service')) {
    switch -Regex ((Split-Path $RepoRoot -Leaf)) {
        'fakeprint' { $Service = 'Print' }
        'fakescan' { $Service = 'Scan' }
        default { $Service = 'All' }
    }
}

# name, url, service, extract-under-docs/reference
$Packages = @(
    @{
        Name    = 'print-v2.0'
        Url     = 'https://go.microsoft.com/fwlink/p/?LinkId=534008'
        Service = 'Print'
        Extract = 'upstream/print-v2'
    },
    @{
        Name    = 'print-v1.2'
        Url     = 'https://download.microsoft.com/download/E/9/7/E974CFCB-4B3B-40CC-AF92-4F7F84477F0B/Printer.zip'
        Service = 'Print'
        Extract = 'schema'
    },
    @{
        Name    = 'print-device'
        Url     = 'https://download.microsoft.com/download/9/c/5/9c5b2167-8017-4bae-9fde-d599bac8184a/PrintDevice.zip'
        Service = 'Print'
        Extract = 'schema'
    },
    @{
        Name    = 'scan-service'
        Url     = 'https://download.microsoft.com/download/9/C/5/9C5B2167-8017-4BAE-9FDE-D599BAC8184A/ScanService.zip'
        Service = 'Scan'
        Extract = 'schema'
    },
    @{
        Name    = 'scan-device'
        Url     = 'https://download.microsoft.com/download/9/c/5/9c5b2167-8017-4bae-9fde-d599bac8184a/ScanDevice.zip'
        Service = 'Scan'
        Extract = 'schema'
    }
)

function Save-OfficialZip {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$Url,
        [switch]$Force
    )
    $dest = Join-Path $Upstream "$Name.zip"
    if ((Test-Path -LiteralPath $dest) -and -not $Force) {
        Write-Host "have $Name ($dest)"
        return $dest
    }
    Write-Host "GET $Url"
    New-Item -ItemType Directory -Force -Path $Upstream | Out-Null
    Invoke-WebRequest -Uri $Url -OutFile $dest -UseBasicParsing
    $size = (Get-Item -LiteralPath $dest).Length
    $head = [System.IO.File]::ReadAllBytes($dest)[0..1]
    if ($head[0] -ne 0x50 -or $head[1] -ne 0x4B) {
        throw "$Name is not a zip ($size bytes from $Url). Left at $dest"
    }
    Write-Host "saved $Name ($size bytes)"
    return $dest
}

function Expand-SchemaEntries {
    param(
        [Parameter(Mandatory)][string]$ZipPath,
        [Parameter(Mandatory)][string]$DestDir
    )
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
    $zip = [System.IO.Compression.ZipFile]::OpenRead($ZipPath)
    try {
        $count = 0
        foreach ($entry in $zip.Entries) {
            if ($entry.FullName.EndsWith('/')) { continue }
            if ($entry.Name -notmatch '\.(xsd|wsdl)$') { continue }
            $out = Join-Path $DestDir $entry.Name
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $out, $true)
            Write-Host "  $($entry.Name)"
            $count++
        }
        if ($count -eq 0) {
            throw "no .xsd or .wsdl entries in $ZipPath"
        }
    }
    finally {
        $zip.Dispose()
    }
}

function Test-CanPrompt {
    $commandLine = [Environment]::GetCommandLineArgs() -join ' '
    if ($commandLine -match '(?i)(^|\s)-NonInteractive(\s|$)') {
        return $false
    }
    try {
        return -not [Console]::IsInputRedirected
    }
    catch {
        return $false
    }
}

function Write-MicrosoftLicenseNotice {
    Write-Host ''
    Write-Host 'Microsoft Non-Confidential Technical Documentation License Agreement (Non-Standard)'
    Write-Host ''
    Write-Host 'This script downloads Microsoft specification packages from Microsoft.'
    Write-Host 'The agreement is the Word document inside each zip. Microsoft says that if you'
    Write-Host 'access that agreement while downloading, you agree by clicking "I ACCEPT", by'
    Write-Host 'downloading, or by using the materials.'
    Write-Host ''
    Write-Host 'You may review the materials only as a reference for your own implementation.'
    Write-Host 'You may not duplicate any part of them, remove the agreement or its notices,'
    Write-Host 'or give any part of them to anyone else. The zip is kept so the agreement stays'
    Write-Host 'with the files. Nothing from the package is committed.'
    Write-Host ''
}

function Confirm-MicrosoftLicense {
    $marker = Join-Path $Upstream '.license-accepted'
    if (Test-Path -LiteralPath $marker) {
        Write-Host "Microsoft license already accepted ($marker)"
        return
    }
    Write-MicrosoftLicenseNotice
    if ($AcceptLicense) {
        Write-Host 'Accepted with -AcceptLicense.'
    }
    elseif (-not (Test-CanPrompt)) {
        Write-Host 'Not downloaded. No prompt is available. Run this script in a terminal and type ACCEPT, or pass -AcceptLicense if you accept the agreement.'
        exit 2
    }
    else {
        $answer = Read-Host 'Type ACCEPT to download'
        if ($answer -cne 'ACCEPT') {
            Write-Host 'Not downloaded.'
            exit 2
        }
    }
    New-Item -ItemType Directory -Force -Path $Upstream | Out-Null
    Set-Content -LiteralPath $marker -Value ("accepted " + [DateTime]::UtcNow.ToString('o')) -Encoding ascii
}

Confirm-MicrosoftLicense

Write-Host "service: $Service"
New-Item -ItemType Directory -Force -Path $Upstream | Out-Null

foreach ($package in $Packages) {
    $zip = Save-OfficialZip -Name $package.Name -Url $package.Url -Force:$Force
    $selected = $Service -eq 'All' -or $Service -eq $package.Service
    if (-not $selected) { continue }
    $dest = $Reference
    foreach ($part in ($package.Extract -split '[\\/]')) {
        $dest = Join-Path $dest $part
    }
    if ($Service -eq 'All' -and $package.Extract -eq 'schema') {
        $dest = Join-Path $dest $package.Service.ToLower()
    }
    Write-Host "extract $($package.Name) -> $dest"
    Expand-SchemaEntries -ZipPath $zip -DestDir $dest
}
