#Requires -Version 5.1
<#
.SYNOPSIS
  Helpers to probe Fake WSD printer discovery on Windows (status, FD, AEP, PnP).
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:DefaultBaseUrl = 'http://127.0.0.1:3921'
$script:PrintDeviceType = 'http://schemas.microsoft.com/windows/2006/08/wdp/print/PrintDeviceType'

function Resolve-FakeprintBaseUrl {
    param([string]$BaseUrl)
    if ([string]::IsNullOrWhiteSpace($BaseUrl)) { return $script:DefaultBaseUrl }
    return $BaseUrl.TrimEnd('/')
}

function Test-FakeprintLooksLike {
    <#
    .SYNOPSIS
      True for fakeprint identity hints: name contains "fakeprint" / "Fake WSD printer",
      uuid …000f3, or a port/URL contains :3921. Does NOT treat generic PrintService
      alone as fakeprint (real Brother WSD printers use that path too).
    #>
    [CmdletBinding()]
    param(
        [string]$Name,
        [string]$Port,
        [string[]]$XAddrs
    )
    $parts = New-Object System.Collections.Generic.List[string]
    if (-not [string]::IsNullOrWhiteSpace($Name)) { [void]$parts.Add($Name) }
    if (-not [string]::IsNullOrWhiteSpace($Port)) { [void]$parts.Add($Port) }
    foreach ($x in @($XAddrs)) {
        if (-not [string]::IsNullOrWhiteSpace($x)) { [void]$parts.Add($x) }
    }
    if ($parts.Count -eq 0) { return $false }
    $joined = ($parts -join ' ')
    return [bool](
        ($joined -match '(?i)fakeprint') -or
        ($joined -match '(?i)Fake WSD printer') -or
        ($joined -match ':3921') -or
        ($joined -match '(?i)00000000-0000-4000-8000-0000000000f3')
    )
}

function Get-FakeprintStatus {
    [CmdletBinding()]
    param([string]$BaseUrl = $script:DefaultBaseUrl)
    $BaseUrl = Resolve-FakeprintBaseUrl $BaseUrl
    Invoke-RestMethod -Uri "$BaseUrl/v1/status" -TimeoutSec 5
}

function Test-FakeprintTransferGet {
    <#
    .SYNOPSIS
      POST WS-Transfer Get to /WebServices/Device; check Printers + VEN_FAKE&DEV_WSDPRINT.
    #>
    [CmdletBinding()]
    param(
        [string]$DeviceUrl = 'http://192.168.1.70:3921/WebServices/Device'
    )
    $body = @"
<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/09/transfer/Get</wsa:Action>
    <wsa:MessageID>urn:uuid:$([guid]::NewGuid())</wsa:MessageID>
    <wsa:To>$DeviceUrl</wsa:To>
    <wsa:ReplyTo><wsa:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:Address></wsa:ReplyTo>
  </soap:Header>
  <soap:Body/>
</soap:Envelope>
"@
    $r = Invoke-WebRequest -Uri $DeviceUrl -Method POST `
        -ContentType 'application/soap+xml; charset=utf-8' `
        -Body $body -TimeoutSec 10 -UseBasicParsing
    $text = [string]$r.Content
    [pscustomobject]@{
        StatusCode = [int]$r.StatusCode
        Ok = ($r.StatusCode -eq 200 -and $text -match 'VEN_FAKE' -and $text -match 'WSDPRINT')
        HasPrinters = [bool]($text -match 'Printers')
        HasHardwareId = [bool]($text -match 'VEN_FAKE(?:&amp;|&)DEV_WSDPRINT')
        HasFriendlyName = [bool]($text -match 'Fake WSD printer')
        ByteLength = $text.Length
    }
}

function Open-FakeprintAddDevice {
    <#
    .SYNOPSIS
      Open Windows Settings -> Printers & scanners for Add device.
    .NOTES
      Opens only ms-settings:printers (do not also open Bluetooth - that steals
      focus). When fakeprint is discoverable it appears under "Add a printer or
      scanner" (e.g. "Fake WSD printer").
    #>
    [CmdletBinding()]
    param()
    Write-Host @'

>>> Person required - Add printer for WSDMON <<<
  Opening: Settings -> Printers & scanners
    -> under "Add a printer or scanner", click  Add device  next to
      Fake WSD printer
  Then print a test page and check:  Invoke-RestMethod http://127.0.0.1:3921/v1/jobs

'@
    Start-Process 'ms-settings:printers'
}

function Get-FakeprintPrinters {
    <#
    .SYNOPSIS
      Get-Printer rows that look like fakeprint (installed WSDMON queue).
    #>
    [CmdletBinding()]
    param()
    @(Get-Printer -ErrorAction SilentlyContinue | ForEach-Object {
        $looks = Test-FakeprintLooksLike -Name $_.Name -Port $_.PortName -XAddrs @($_.Location, $_.DriverName)
        [pscustomobject]@{
            Name = $_.Name
            DriverName = $_.DriverName
            PortName = $_.PortName
            Location = $_.Location
            LooksLikeFakeprint = [bool]$looks
        }
    } | Where-Object { $_.LooksLikeFakeprint })
}

function Get-FakeprintPnpDevices {
    <#
    .SYNOPSIS
      PnP Printer / WSDPrintDevice / DAFWSDPROVIDER nodes; -FakeprintOnly filters identity.
    #>
    [CmdletBinding()]
    param([switch]$FakeprintOnly)
    $rows = @()
    try {
        $rows = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
            $_.Class -in @('Printer', 'WSDPrintDevice') -or
            $_.InstanceId -match '(?i)DAFWSDPROVIDER|\\WSD|PRINTENUM' -or
            $_.FriendlyName -match '(?i)Fake WSD|fakeprint'
        } | ForEach-Object {
            [pscustomobject]@{
                Status = $_.Status
                Class = $_.Class
                Name = $_.FriendlyName
                InstanceId = $_.InstanceId
                LooksLikeFakeprint = (Test-FakeprintLooksLike -Name $_.FriendlyName -Port $_.InstanceId)
            }
        })
    } catch {
        Write-Warning "Get-PnpDevice failed: $_"
    }
    if ($FakeprintOnly) {
        return @($rows | Where-Object { $_.LooksLikeFakeprint })
    }
    return $rows
}

function Initialize-FakeprintFunctionDiscoveryTypes {
    if ('Fakeprint.Fd.FdQuery' -as [type]) { return $true }
    $cs = Join-Path $PSScriptRoot 'Fakeprint.FunctionDiscovery.cs'
    if (-not (Test-Path -LiteralPath $cs)) {
        Write-Warning "Missing $cs"
        return $false
    }
    try {
        Add-Type -Path $cs -ErrorAction Stop
        return $true
    } catch {
        if ('Fakeprint.Fd.FdQuery' -as [type]) { return $true }
        Write-Warning "Add-Type Function Discovery helpers failed: $_"
        return $false
    }
}

function Get-FakeprintFunctionDiscovery {
    <#
    .SYNOPSIS
      Query Windows Function Discovery for WSD / network device instances.
    .DESCRIPTION
      Same COM surface as fakescan: Provider\Microsoft.Networking.WSD and
      Layered\Microsoft.Networking.Devices (sub WSD). Filters FakeprintHits by
      Test-FakeprintLooksLike (:3921 / Fake WSD printer / uuid …000f3).
    #>
    [CmdletBinding()]
    param(
        [int]$TimeoutMs = 8000,
        [string]$DirectedAddress,
        [string]$TypeConstraint,
        [switch]$WsdOnly
    )

    if (-not (Initialize-FakeprintFunctionDiscoveryTypes)) {
        return [pscustomobject]@{
            Ok = $false
            Error = 'Function Discovery types unavailable'
            Hits = @()
            FakeprintHits = @()
            Queries = @()
        }
    }

    $categories = @(
        @{ Cat = 'Provider\Microsoft.Networking.WSD'; Sub = '' },
        @{ Cat = 'Layered\Microsoft.Networking.Devices'; Sub = 'WSD' }
    )
    if ($WsdOnly) {
        $categories = @(@{ Cat = 'Provider\Microsoft.Networking.WSD'; Sub = '' })
    }

    $all = New-Object System.Collections.Generic.List[object]
    $queries = New-Object System.Collections.Generic.List[object]
    $typeArg = if ([string]::IsNullOrWhiteSpace($TypeConstraint)) { '' } else { [string]$TypeConstraint }
    $dirArg = if ([string]::IsNullOrWhiteSpace($DirectedAddress)) { '' } else { [string]$DirectedAddress }
    foreach ($c in $categories) {
        try {
            $catArg = [string]$c['Cat']
            $subArg = [string]$c['Sub']
            if ($null -eq $subArg) { $subArg = '' }
            $mi = [Fakeprint.Fd.FdQuery].GetMethod('QueryAsync')
            try {
                $r = $mi.Invoke($null, [object[]]@(
                    $catArg,
                    $subArg,
                    $typeArg,
                    $dirArg,
                    [int]$TimeoutMs
                ))
            } catch [System.Reflection.TargetInvocationException] {
                throw $_.Exception.InnerException
            }
            [void]$queries.Add([pscustomobject]@{
                Category = $catArg
                SubCategory = $subArg
                ExecuteHr = ('0x{0:X8}' -f [int]$r.ExecuteHr)
                LastError = $r.LastError
                HitCount = @($r.Hits).Count
            })
            foreach ($h in @($r.Hits)) {
                $xaddrs = New-Object System.Collections.Generic.List[string]
                if (-not [string]::IsNullOrWhiteSpace([string]$h.XAddrs)) {
                    foreach ($p in ([string]$h.XAddrs -split '\s+')) {
                        if ($p) { [void]$xaddrs.Add($p) }
                    }
                }
                if (-not [string]::IsNullOrWhiteSpace([string]$h.ServiceAddress)) {
                    foreach ($p in ([string]$h.ServiceAddress -split '\s+')) {
                        if ($p) { [void]$xaddrs.Add($p) }
                    }
                }
                $xarr = @($xaddrs.ToArray())
                $looks = Test-FakeprintLooksLike -Name ([string]$h.FriendlyName) -Port ([string]$h.ModelName) -XAddrs $xarr
                if (-not $looks) {
                    $extra = @()
                    if ($h.GlobalIdentity) { $extra += [string]$h.GlobalIdentity }
                    if ($h.IpAddress) { $extra += [string]$h.IpAddress }
                    if ($h.Id) { $extra += [string]$h.Id }
                    $looks = Test-FakeprintLooksLike -Name ([string]$h.Id) -XAddrs $extra
                }
                [void]$all.Add([pscustomobject]@{
                    Category = [string]$h.Category
                    Source = [string]$h.Source
                    Id = [string]$h.Id
                    FriendlyName = [string]$h.FriendlyName
                    Manufacturer = [string]$h.Manufacturer
                    ModelName = [string]$h.ModelName
                    GlobalIdentity = [string]$h.GlobalIdentity
                    Types = [string]$h.Types
                    XAddrs = [string]$h.XAddrs
                    ServiceAddress = [string]$h.ServiceAddress
                    IpAddress = [string]$h.IpAddress
                    DeviceCategory = [string]$h.DeviceCategory
                    Installable = [string]$h.Installable
                    LooksLikeFakeprint = [bool]$looks
                })
            }
        } catch {
            [void]$queries.Add([pscustomobject]@{
                Category = [string]$c['Cat']
                SubCategory = [string]$c['Sub']
                ExecuteHr = $null
                LastError = "$_"
                HitCount = 0
            })
        }
    }

    $allArr = $all.ToArray()
    $queryArr = $queries.ToArray()
    $uniq = New-Object System.Collections.Generic.List[object]
    $seen = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($row in $allArr) {
        $idPart = ''
        $namePart = ''
        if ($null -ne $row.Id) { $idPart = $row.Id.ToString() }
        if ($null -ne $row.FriendlyName) { $namePart = $row.FriendlyName.ToString() }
        $key = $idPart + '|' + $namePart
        if (-not $seen.Add($key)) { continue }
        [void]$uniq.Add($row)
    }
    $fakeArr = @($uniq.ToArray() | Where-Object { $_.LooksLikeFakeprint -eq $true })
    $installableFake = $false
    foreach ($row in $fakeArr) {
        if ($row.Installable -eq 'true') { $installableFake = $true; break }
    }
    [pscustomobject]@{
        Ok = $true
        Error = $null
        Hits = $uniq.ToArray()
        FakeprintHits = $fakeArr
        FakeprintPresent = ($fakeArr.Count -gt 0)
        InstallableFakeprint = $installableFake
        Queries = $queryArr
    }
}

function Find-FakeprintFunctionDiscovery {
    [CmdletBinding()]
    param(
        [int]$TimeoutMs = 8000,
        [string]$DirectedAddress,
        [string]$TypeConstraint
    )
    $params = @{ TimeoutMs = $TimeoutMs }
    if (-not [string]::IsNullOrWhiteSpace($DirectedAddress)) {
        $params['DirectedAddress'] = $DirectedAddress
    }
    if (-not [string]::IsNullOrWhiteSpace($TypeConstraint)) {
        $params['TypeConstraint'] = $TypeConstraint
    }
    $r = Get-FakeprintFunctionDiscovery @params
    return @($r.FakeprintHits)
}

function Get-FakeprintAssociationEndpoints {
    <#
    .SYNOPSIS
      Enumerate WinRT WSD AssociationEndpoint (AEP) devices (paired + unpaired).
    #>
    [CmdletBinding()]
    param([int]$TimeoutMs = 25000)

    try {
        Add-Type -AssemblyName System.Runtime.WindowsRuntime -ErrorAction Stop
    } catch {
        return [pscustomobject]@{
            Ok = $false
            Error = "System.Runtime.WindowsRuntime unavailable: $_"
            Hits = @()
            FakeprintHits = @()
            FakeprintPresent = $false
            CanPairFakeprint = $false
        }
    }

    $asTaskGeneric = [System.WindowsRuntimeSystemExtensions].GetMethods() |
        Where-Object {
            $_.Name -eq 'AsTask' -and $_.IsGenericMethod -and $_.GetParameters().Count -eq 1
        } |
        Select-Object -First 1
    if (-not $asTaskGeneric) {
        return [pscustomobject]@{
            Ok = $false
            Error = 'AsTask extension method not found'
            Hits = @()
            FakeprintHits = @()
            FakeprintPresent = $false
            CanPairFakeprint = $false
        }
    }

    $waitMs = [Math]::Max(1000, [int]$TimeoutMs)
    try {
        $null = [Windows.Devices.Enumeration.DeviceInformation, Windows, ContentType = WindowsRuntime]
        $aqs = 'System.Devices.Aep.ProtocolId:="{782232aa-a2f9-4993-971b-aedc551346b0}"'
        $kind = [Windows.Devices.Enumeration.DeviceInformationKind]::AssociationEndpoint
        $op = [Windows.Devices.Enumeration.DeviceInformation]::FindAllAsync($aqs, $null, $kind)
        $asTask = $asTaskGeneric.MakeGenericMethod([Windows.Devices.Enumeration.DeviceInformationCollection])
        $netTask = $asTask.Invoke($null, @($op))
        if (-not $netTask.Wait($waitMs)) {
            throw "WinRT FindAllAsync timed out after ${waitMs}ms"
        }
        if ($netTask.IsFaulted) { throw $netTask.Exception.InnerException }
        $devs = $netTask.Result
        $hits = @(foreach ($d in $devs) {
            $name = [string]$d.Name
            $id = [string]$d.Id
            $looks = Test-FakeprintLooksLike -Name $name -Port $id
            [pscustomobject]@{
                Name = $name
                Id = $id
                IsPaired = [bool]$d.Pairing.IsPaired
                CanPair = [bool]$d.Pairing.CanPair
                LooksLikeFakeprint = [bool]$looks
            }
        })
        $fake = @($hits | Where-Object { $_.LooksLikeFakeprint })
        $canPair = @($fake | Where-Object { $_.CanPair -and -not $_.IsPaired })
        [pscustomobject]@{
            Ok = $true
            Error = $null
            Hits = $hits
            FakeprintHits = $fake
            FakeprintPresent = ($fake.Count -gt 0)
            CanPairFakeprint = ($canPair.Count -gt 0)
        }
    } catch {
        [pscustomobject]@{
            Ok = $false
            Error = "$_"
            Hits = @()
            FakeprintHits = @()
            FakeprintPresent = $false
            CanPairFakeprint = $false
        }
    }
}

function Get-FakeprintDiscoveryReport {
    <#
    .SYNOPSIS
      One-shot status + Transfer Get + printers + PnP + FD + AEP summary for lab notes.
    #>
    [CmdletBinding()]
    param(
        [string]$BaseUrl = $script:DefaultBaseUrl,
        [string]$DeviceUrl = 'http://192.168.1.70:3921/WebServices/Device',
        [int]$FdTimeoutMs = 10000,
        [int]$AepTimeoutMs = 25000,
        [switch]$OpenAddUi
    )

    $status = $null
    $statusErr = $null
    try { $status = Get-FakeprintStatus -BaseUrl $BaseUrl } catch { $statusErr = "$_" }

    $xfer = $null
    $xferErr = $null
    try { $xfer = Test-FakeprintTransferGet -DeviceUrl $DeviceUrl } catch { $xferErr = "$_" }

    $printers = @(Get-FakeprintPrinters)
    $pnp = @(Get-FakeprintPnpDevices)
    $pnpFake = @($pnp | Where-Object { $_.LooksLikeFakeprint })

    $fd = Get-FakeprintFunctionDiscovery -TimeoutMs $FdTimeoutMs
    $fdPrintType = Get-FakeprintFunctionDiscovery -TimeoutMs $FdTimeoutMs `
        -TypeConstraint $script:PrintDeviceType -WsdOnly
    $aep = Get-FakeprintAssociationEndpoints -TimeoutMs $AepTimeoutMs

    if ($OpenAddUi) { Open-FakeprintAddDevice }

    [pscustomobject]@{
        Status = $status
        StatusError = $statusErr
        TransferGet = $xfer
        TransferGetError = $xferErr
        InstalledPrinters = $printers
        PnpAllWsdish = $pnp
        PnpFakeprint = $pnpFake
        FunctionDiscovery = $fd
        FunctionDiscoveryPrintType = $fdPrintType
        AssociationEndpoints = $aep
        VisibleInFd = [bool]$fd.FakeprintPresent
        InstallableInFd = [bool]$fd.InstallableFakeprint
        VisibleInAep = [bool]$aep.FakeprintPresent
        CanPairInAep = [bool]$aep.CanPairFakeprint
        InstalledQueue = ($printers.Count -gt 0)
        InstalledPnp = ($pnpFake.Count -gt 0)
    }
}

Export-ModuleMember -Function @(
    'Resolve-FakeprintBaseUrl',
    'Test-FakeprintLooksLike',
    'Get-FakeprintStatus',
    'Test-FakeprintTransferGet',
    'Open-FakeprintAddDevice',
    'Get-FakeprintPrinters',
    'Get-FakeprintPnpDevices',
    'Get-FakeprintFunctionDiscovery',
    'Find-FakeprintFunctionDiscovery',
    'Get-FakeprintAssociationEndpoints',
    'Get-FakeprintDiscoveryReport'
)
