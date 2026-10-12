#Requires -Version 5.1
<#
.SYNOPSIS
  Helpers to probe Fake WSD printer discovery on Windows (status, FD, AEP, PnP).
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:DefaultBaseUrl = 'http://127.0.0.1:3921'

# -- Invoke-LabAttention ----------------------------------------------------
# Private helper (not exported). Called from Open-FakeprintAddDevice and the
# ensure flow whenever added.pnp is false and no silent install has succeeded.
#
# Default: yellow banner + [Console]::Beep(1000,300) + BEL char + WinForms
#          NotifyIcon balloon (held 3 s so Windows can show it).
# -Loud:   also FlashWindowEx on the first parent-process window handle.
# -Quiet:  banner only; suppresses beep and balloon.

function Invoke-LabAttention {
    [CmdletBinding()]
    param(
        # One-line context printed inside the banner.
        [string]$Detail = 'Add device next to "Fake WSD printer".',
        # Add flashy taskbar flashing via FlashWindowEx.
        [switch]$Loud,
        # Suppress beep and balloon; banner still prints.
        [switch]$Quiet
    )

    # -- 1. Yellow console banner (always) --------------------------------
    $line  = '=' * 66
    $block = @"

$line
  !!  fakeprint -- PERSON REQUIRED -- Add device
      $Detail
      Settings -> Printers & scanners is opening now.
      Click  Add device  next to Fake WSD printer.
      Verify when done:  Get-FakeprintPrinters
$line

"@
    try {
        $prev = $host.UI.RawUI.ForegroundColor
        $host.UI.RawUI.ForegroundColor = 'Yellow'
        Write-Host $block
        $host.UI.RawUI.ForegroundColor = $prev
    } catch {
        Write-Host $block
    }

    if (-not $Quiet) {
        # -- 2. Audible beep ---------------------------------------------
        try { [Console]::Beep(1000, 300) } catch { }
        # -- 4. BEL character alongside beep ------------------------------
        try { [Console]::Write([char]0x7) } catch { }

        # -- 3. WinForms NotifyIcon balloon -------------------------------
        try {
            Add-Type -AssemblyName System.Windows.Forms -ErrorAction Stop
            $ni          = New-Object System.Windows.Forms.NotifyIcon
            $ni.Icon     = [System.Drawing.SystemIcons]::Warning
            $ni.Visible  = $true
            $ni.ShowBalloonTip(
                3000,
                'fakeprint -- Add device required',
                $Detail,
                [System.Windows.Forms.ToolTipIcon]::Warning
            )
            Start-Sleep -Milliseconds 3000
            $ni.Dispose()
        } catch {
            Write-Verbose "Invoke-LabAttention balloon: $_"
        }
    }

    # -- Opt-in: FlashWindowEx on parent terminal window ------------------
    if ($Loud) {
        try {
            if (-not ('FakeprintLabFlash' -as [type])) {
                Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class FakeprintLabFlash {
    [StructLayout(LayoutKind.Sequential)]
    public struct FLASHWINFO {
        public uint  cbSize;
        public IntPtr hwnd;
        public uint  dwFlags;
        public uint  uCount;
        public uint  dwTimeout;
    }
    [DllImport("user32.dll")]
    public static extern bool FlashWindowEx(ref FLASHWINFO pwfi);
    public const uint FLASHW_ALL      = 3;
    public const uint FLASHW_TIMERNOFG = 12;
    public static void Flash(IntPtr hwnd) {
        if (hwnd == IntPtr.Zero) return;
        var fi = new FLASHWINFO {
            cbSize   = (uint)System.Runtime.InteropServices.Marshal.SizeOf(typeof(FLASHWINFO)),
            hwnd     = hwnd,
            dwFlags  = FLASHW_ALL | FLASHW_TIMERNOFG,
            uCount   = 6,
            dwTimeout = 0
        };
        FlashWindowEx(ref fi);
    }
}
'@ -ErrorAction Stop
            }
            # Walk parent process tree to find the first window handle.
            $walkPid = $PID
            $seen    = [System.Collections.Generic.HashSet[int]]::new()
            $hwnd    = [IntPtr]::Zero
            while ($walkPid -gt 0 -and $seen.Add($walkPid)) {
                try {
                    $p = [System.Diagnostics.Process]::GetProcessById($walkPid)
                    if ($p.MainWindowHandle -ne [IntPtr]::Zero) {
                        $hwnd = $p.MainWindowHandle; break
                    }
                } catch { }
                try {
                    $cimRow  = Get-CimInstance -ClassName Win32_Process `
                                   -Filter "ProcessId = $walkPid" -ErrorAction Stop
                    $walkPid = [int]$cimRow.ParentProcessId
                } catch { break }
            }
            [FakeprintLabFlash]::Flash($hwnd)
        } catch {
            Write-Verbose "Invoke-LabAttention FlashWindowEx: $_"
        }
    }
}

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
      uuid ...000f3, or a port/URL contains :3921. Does NOT treat generic PrintService
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
        ($joined -match '(?i)Fakeprint-(IPP|JD)-') -or
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
    .DESCRIPTION
      Fires Invoke-LabAttention (banner + beep + balloon by default) before
      opening ms-settings:printers. Only called when added.pnp is false and
      no silent install path has already succeeded.
      -Loud:  also flash the parent terminal window (FlashWindowEx).
      -Quiet: banner only; no beep or balloon.
    .NOTES
      Opens only ms-settings:printers (do not also open Bluetooth - that steals
      focus). Silent CLI install is unsupported for WSDMON.
    #>
    [CmdletBinding()]
    param(
        [switch]$Loud,
        [switch]$Quiet
    )
    Invoke-LabAttention -Detail 'Add device next to "Fake WSD printer".' `
        -Loud:$Loud -Quiet:$Quiet
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
      Test-FakeprintLooksLike (:3921 / Fake WSD printer / uuid ...000f3).
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

function Disconnect-FakeprintAssociationEndpoint {
    <#
    .SYNOPSIS
      WinRT UnpairAsync on a paired fakeprint WSD AssociationEndpoint.
    .DESCRIPTION
      After WSDPrintDevice install without a spooler queue, AEP can stay
      IsPaired=true while Settings Add only loops Device Transfer Get
      ("Acquiring data"). Unpair clears the stale association so Add can
      re-run against current DeviceId (CID:MS_IPP) / PresentationUrl metadata.
    #>
    [CmdletBinding()]
    param(
        [int]$TimeoutMs = 25000,
        [int]$UnpairTimeoutMs = 120000
    )

    $aep = Get-FakeprintAssociationEndpoints -TimeoutMs $TimeoutMs
    if (-not $aep.Ok) {
        return [pscustomobject]@{
            Ok = $false
            Error = $aep.Error
            Status = $null
            DeviceId = $null
            Name = $null
            IsPairedAfter = $null
        }
    }
    $cand = @($aep.FakeprintHits | Where-Object { $_.IsPaired } | Select-Object -First 1)
    if (-not $cand) {
        return [pscustomobject]@{
            Ok = $true
            Error = $null
            Status = 'AlreadyUnpaired'
            DeviceId = $null
            Name = $null
            IsPairedAfter = $false
        }
    }

    try {
        Add-Type -AssemblyName System.Runtime.WindowsRuntime -ErrorAction Stop
        $asTaskGeneric = [System.WindowsRuntimeSystemExtensions].GetMethods() |
            Where-Object {
                $_.Name -eq 'AsTask' -and $_.IsGenericMethod -and $_.GetParameters().Count -eq 1
            } |
            Select-Object -First 1
        $null = [Windows.Devices.Enumeration.DeviceInformation, Windows, ContentType = WindowsRuntime]
        $aqs = 'System.Devices.Aep.ProtocolId:="{782232aa-a2f9-4993-971b-aedc551346b0}"'
        $kind = [Windows.Devices.Enumeration.DeviceInformationKind]::AssociationEndpoint
        $findOp = [Windows.Devices.Enumeration.DeviceInformation]::FindAllAsync($aqs, $null, $kind)
        $findTask = $asTaskGeneric.MakeGenericMethod(
            [Windows.Devices.Enumeration.DeviceInformationCollection]
        ).Invoke($null, @($findOp))
        if (-not $findTask.Wait([Math]::Max(1000, $TimeoutMs))) {
            throw 'FindAllAsync timed out'
        }
        if ($findTask.IsFaulted) { throw $findTask.Exception.InnerException }
        $target = $null
        foreach ($d in $findTask.Result) {
            if ([string]$d.Id -eq [string]$cand.Id) { $target = $d; break }
        }
        if (-not $target) {
            throw "AEP id vanished before UnpairAsync: $($cand.Id)"
        }
        $unpairOp = $target.Pairing.UnpairAsync()
        $unpairTask = $asTaskGeneric.MakeGenericMethod(
            [Windows.Devices.Enumeration.DeviceUnpairingResult]
        ).Invoke($null, @($unpairOp))
        if (-not $unpairTask.Wait([Math]::Max(1000, $UnpairTimeoutMs))) {
            throw 'UnpairAsync timed out'
        }
        if ($unpairTask.IsFaulted) { throw $unpairTask.Exception.InnerException }
        $status = [string]$unpairTask.Result.Status
        Start-Sleep -Seconds 2
        $after = Get-FakeprintAssociationEndpoints -TimeoutMs $TimeoutMs
        $still = @($after.FakeprintHits | Where-Object { $_.Id -eq $cand.Id -and $_.IsPaired })
        [pscustomobject]@{
            Ok = ($status -eq 'Unpaired' -or $still.Count -eq 0)
            Error = if ($status -eq 'Unpaired' -or $still.Count -eq 0) { $null } else { "UnpairAsync status=$status" }
            Status = $status
            DeviceId = [string]$cand.Id
            Name = [string]$cand.Name
            IsPairedAfter = ($still.Count -gt 0)
        }
    } catch {
        [pscustomobject]@{
            Ok = $false
            Error = "$_"
            Status = $null
            DeviceId = [string]$cand.Id
            Name = [string]$cand.Name
            IsPairedAfter = $true
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

# ---------------------------------------------------------------------------
# Device registry (%LOCALAPPDATA%\fakeprint\devices.json)
# Never touches %LOCALAPPDATA%\fakescan -- fakeprint's registry only.
# ---------------------------------------------------------------------------

function Get-FakeprintRegistryPath {
    $dir = Join-Path $env:LOCALAPPDATA 'fakeprint'
    if (-not (Test-Path -LiteralPath $dir)) {
        $null = New-Item -ItemType Directory -Path $dir -Force
    }
    Join-Path $dir 'devices.json'
}

$script:DefaultDeviceRow = [ordered]@{
    name          = 'default'
    listen        = '0.0.0.0:3921'
    advertise     = ''
    device_uuid   = '00000000-0000-4000-8000-0000000000f3'
    profile       = 'generic-wsd'
    exe           = ''
    autostart     = $false
    added         = @{ pnp = $false }
}

function Get-FakeprintRegistry {
    <#
    .SYNOPSIS
      Load (or create) %LOCALAPPDATA%\fakeprint\devices.json.
    #>
    [CmdletBinding()]
    param()
    $path = Get-FakeprintRegistryPath
    if (-not (Test-Path -LiteralPath $path)) {
        $reg = [ordered]@{
            advertise_default = ''
            port_policy       = @{ base = 3921; step = 2 }
            devices           = @($script:DefaultDeviceRow)
        }
        $reg | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $path -Encoding UTF8
    }
    $raw = Get-Content -LiteralPath $path -Raw -Encoding UTF8
    $raw | ConvertFrom-Json
}

function Save-FakeprintRegistry {
    <#
    .SYNOPSIS
      Write the registry object back to devices.json.
    #>
    [CmdletBinding()]
    param([Parameter(Mandatory)][psobject]$Registry)
    $path = Get-FakeprintRegistryPath
    $Registry | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $path -Encoding UTF8
    Write-Verbose "Saved $path"
}

function Get-FakeprintDevices {
    <#
    .SYNOPSIS
      List all device rows from this product's registry.
    #>
    [CmdletBinding()]
    param()
    $reg = Get-FakeprintRegistry
    @($reg.devices)
}

function Add-FakeprintDevice {
    <#
    .SYNOPSIS
      Add a new named device row. Port must be odd (fakeprint band).
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Name,
        [string]$Listen,
        [string]$Advertise = '',
        [string]$Profile = 'generic-wsd',
        [string]$Exe = '',
        [switch]$Autostart
    )
    $reg = Get-FakeprintRegistry
    $existing = @($reg.devices | Where-Object { $_.name -eq $Name })
    if ($existing.Count -gt 0) {
        throw "Device '$Name' already exists in fakeprint registry. Use Save-FakeprintDevice to update."
    }
    # Assign next odd port if not given
    if ([string]::IsNullOrWhiteSpace($Listen)) {
        $usedPorts = @($reg.devices | ForEach-Object {
            $_.listen -replace '^.*:', ''
        } | Where-Object { $_ -match '^\d+$' } | ForEach-Object { [int]$_ })
        $base = [int]$reg.port_policy.base
        $step = [int]$reg.port_policy.step
        $port = $base
        while ($usedPorts -contains $port) { $port += $step }
        $Listen = "0.0.0.0:$port"
    } else {
        $portPart = ([int]($Listen -replace '^.*:', ''))
        if ($portPart % 2 -eq 0) {
            throw "fakeprint uses odd ports (3921, 3923, ...); got $Listen. fakescan owns even ports."
        }
    }
    $row = [ordered]@{
        name        = $Name
        listen      = $Listen
        advertise   = $Advertise
        device_uuid = '00000000-0000-4000-8000-0000000000f3'
        profile     = $Profile
        exe         = $Exe
        autostart   = [bool]$Autostart
        added       = @{ pnp = $false }
    }
    $reg.devices += $row
    Save-FakeprintRegistry $reg
    $row
}

function Remove-FakeprintDevice {
    <#
    .SYNOPSIS
      Remove a named device row from the registry (does not stop the process).
    #>
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$Name)
    $reg = Get-FakeprintRegistry
    $before = @($reg.devices).Count
    $reg.devices = @($reg.devices | Where-Object { $_.name -ne $Name })
    $after = @($reg.devices).Count
    if ($after -eq $before) { Write-Warning "No device '$Name' found in fakeprint registry." }
    else { Save-FakeprintRegistry $reg; Write-Host "Removed '$Name' from fakeprint registry." }
}

function Save-FakeprintDevice {
    <#
    .SYNOPSIS
      Persist the running process's current config into its registry row.
      Reads /v1/status from the process and writes name/profile back.
    #>
    [CmdletBinding()]
    param(
        [string]$Name = 'default',
        [string]$BaseUrl = $script:DefaultBaseUrl
    )
    $BaseUrl = Resolve-FakeprintBaseUrl $BaseUrl
    $st = Invoke-RestMethod -Uri "$BaseUrl/v1/status" -TimeoutSec 5
    $reg = Get-FakeprintRegistry
    $found = $false
    $reg.devices = @($reg.devices | ForEach-Object {
        if ($_.name -eq $Name) {
            $found = $true
            $_.profile = $st.profile_id
            # Preserve listen/uuid/advertise from existing row
        }
        $_
    })
    if (-not $found) {
        Write-Warning "Row '$Name' not found; use Add-FakeprintDevice first."
        return
    }
    Save-FakeprintRegistry $reg
    Write-Host "Saved '$Name' row (profile=$($st.profile_id))"
}

function Set-FakeprintDeviceAdded {
    <#
    .SYNOPSIS
      Mark a registry row's added.pnp = true after a person clicks Add device in Windows.
    #>
    [CmdletBinding()]
    param([string]$Name = 'default', [bool]$Pnp = $true)
    $reg = Get-FakeprintRegistry
    $found = $false
    $reg.devices = @($reg.devices | ForEach-Object {
        if ($_.name -eq $Name) {
            $found = $true
            if ($null -eq $_.added) { $_ | Add-Member -NotePropertyName added -NotePropertyValue @{} }
            $_.added.pnp = $Pnp
        }
        $_
    })
    if (-not $found) {
        Write-Warning "Row '$Name' not found."
        return
    }
    Save-FakeprintRegistry $reg
    Write-Host "Marked '$Name' added.pnp=$Pnp"
}

# ---------------------------------------------------------------------------
# IPP spooler queue -- non-interactive Add-Printer (admin required).
# Creates a Standard-TCP/IP / Internet-Printing port and a Microsoft IPP
# Class Driver queue without opening Settings > Printers & scanners.
# ---------------------------------------------------------------------------

function Test-FakeprintIsAdmin {
    <#
    .SYNOPSIS
      True when the current session is running as a local administrator.
    #>
    ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator
    )
}

function Start-FakeprintElevatedShell {
    <#
    .SYNOPSIS
      Open an elevated PowerShell window with Fakeprint.psm1 imported.
    .DESCRIPTION
      Elevation unlocks Install-FakeprintIppQueue / Remove-FakeprintIppQueue.
      It does not unlock WSDMON Add device without a person. Never touches fakescan.
    #>
    [CmdletBinding()]
    param(
        [string]$Command = 'Write-Host "Elevated fakeprint shell. Try: Install-FakeprintIppQueue; Get-FakeprintPrinters"'
    )
    $mod = Join-Path $PSScriptRoot 'Fakeprint.psm1'
    $escapedMod = $mod.Replace("'", "''")
    $escapedCmd = $Command.Replace("'", "''")
    $arg = "-NoExit -NoProfile -ExecutionPolicy Bypass -Command `"Import-Module '$escapedMod' -Force; $escapedCmd`""
    Start-Process -FilePath 'powershell.exe' -Verb RunAs -ArgumentList $arg | Out-Null
    Write-Host 'Launched elevated PowerShell (UAC prompt).'
}

function Start-FakeprintElevatedInstallIpp {
    <#
    .SYNOPSIS
      One-shot elevated Install-FakeprintIppQueue via UAC (no lingering shell).
    .DESCRIPTION
      Person approves UAC; then Add-PrinterPort + Microsoft IPP Class Driver run
      as admin. Waits for the queue and marks added.pnp when found.
    #>
    [CmdletBinding()]
    param(
        [string]$Name = 'default',
        [string]$BaseUrl = $script:DefaultBaseUrl,
        [string]$PrinterName,
        [switch]$Force,
        [int]$WaitSeconds = 45
    )

    if (Test-FakeprintIsAdmin) {
        $params = @{ Name = $Name; BaseUrl = $BaseUrl; Force = $Force }
        if (-not [string]::IsNullOrWhiteSpace($PrinterName)) { $params['PrinterName'] = $PrinterName }
        return Install-FakeprintIppQueue @params
    }

    $mod = Join-Path $PSScriptRoot 'Fakeprint.psm1'
    $log = Join-Path $env:TEMP ("fakeprint-ipp-install-{0}.log" -f [guid]::NewGuid().ToString('N'))
    $doneMarker = "$log.done"
    $forceLit = if ($Force) { '$true' } else { '$false' }
    $nameEsc = $Name.Replace("'", "''")
    $baseEsc = $BaseUrl.Replace("'", "''")
    $modEsc = $mod.Replace("'", "''")
    $logEsc = $log.Replace("'", "''")
    $doneEsc = $doneMarker.Replace("'", "''")
    $pnEsc = if ([string]::IsNullOrWhiteSpace($PrinterName)) { '' } else { $PrinterName.Replace("'", "''") }

    $inner = @"
`$ErrorActionPreference = 'Continue'
Import-Module '$modEsc' -Force
`$params = @{ Name = '$nameEsc'; BaseUrl = '$baseEsc'; Force = $forceLit }
if (-not [string]::IsNullOrWhiteSpace('$pnEsc')) { `$params['PrinterName'] = '$pnEsc' }
try {
  Install-FakeprintIppQueue @params *>&1 | Tee-Object -FilePath '$logEsc'
  'OK' | Set-Content -LiteralPath '$doneEsc'
} catch {
  `$_ | Out-String | Set-Content -LiteralPath '$logEsc'
  'FAIL' | Set-Content -LiteralPath '$doneEsc'
}
"@
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($inner))
    Write-Host "Launching elevated IPP install (approve UAC). Log: $log"
    Start-Process -FilePath 'powershell.exe' -Verb RunAs -ArgumentList @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded
    ) | Out-Null

    function Get-FakeprintLabPrinter {
        # Accept:
        #   - Internet Port / http(s) PortName (true IPP)
        #   - TCPMON RAW :9100 with a host-only address (JetDirect fall-back)
        # Reject TCPMON whose PrinterHostAddress still contains "http://" (broken).
        Get-Printer -ErrorAction SilentlyContinue | Where-Object {
            if ($_.Name -notmatch 'Fake' -and $_.PortName -notmatch 'Fakeprint|ipp/print|3921') {
                return $false
            }
            $port = Get-PrinterPort -Name $_.PortName -ErrorAction SilentlyContinue
            if (-not $port) { return $_.PortName -match '^https?://' }
            if ($port.PortMonitor -match 'Internet' -or $_.PortName -match '^https?://') { return $true }
            if ($port.PortMonitor -eq 'TCPMON.DLL') {
                $addr = [string]$port.PrinterHostAddress
                if ($addr -match '^https?://') { return $false }
                return ([int]$port.PortNumber -eq 9100 -and -not [string]::IsNullOrWhiteSpace($addr))
            }
            $false
        } | Select-Object -First 1
    }

    $deadline = (Get-Date).AddSeconds($WaitSeconds)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 800
        if (Test-Path -LiteralPath $doneMarker) { break }
    }

    if (Test-Path -LiteralPath $log) {
        Write-Host '--- elevated install log ---'
        Get-Content -LiteralPath $log -ErrorAction SilentlyContinue | Write-Host
    }
    $ok = $false
    if (Test-Path -LiteralPath $doneMarker) {
        $ok = ((Get-Content -LiteralPath $doneMarker -Raw).Trim() -eq 'OK')
    }
    $hit = Get-FakeprintLabPrinter
    if ($hit) {
        Set-FakeprintDeviceAdded -Name $Name -Pnp $true | Out-Null
        $mon = (Get-PrinterPort -Name $hit.PortName -ErrorAction SilentlyContinue).PortMonitor
        Write-Host ("Lab print queue visible: {0} port={1} monitor={2}" -f $hit.Name, $hit.PortName, $mon)
        $ok = $true
    } else {
        Write-Warning 'No usable Fake print queue (need Internet Port or TCPMON host:9100, not http:// TCPMON).'
    }
    [pscustomobject]@{
        Ok = $ok
        ElevatedLaunch = $true
        Log = $log
        Printer = $hit
    }
}

function Install-FakeprintIppQueue {
    <#
    .SYNOPSIS
      Create an IPP spooler queue for fakeprint non-interactively (admin required).
    .DESCRIPTION
      Reads the advertise address from the running fakeprint instance (/v1/status)
      and installs via Add-Printer -IppURL http://<advertise>:<port>/ipp/print
      (Microsoft IPP Class Driver / Internet Port monitor).

      Do NOT use Add-PrinterPort -PrinterHostAddress with an http URL: that creates
      TCPMON RAW :9100, and jobs stick at Printing/Retained with zero IPP traffic.

      Fall-back: if creation fails (driver missing, access denied) the function
      warns and suggests Open-FakeprintAddDevice for the person-click WSD path.
    .PARAMETER Name
      Registry row name (default: 'default').
    .PARAMETER BaseUrl
      fakeprint control URL (default: http://127.0.0.1:3921).
    .PARAMETER PrinterName
      Spooler queue display name. Defaults to the profile label from /v1/status.
    .PARAMETER Force
      Remove and re-create the port and queue if they already exist.
    .PARAMETER WsdAdd
      Skip IPP install and open Settings -> Printers & scanners instead
      (person-click WSD path). Useful when IPP path is known to fail.
    .EXAMPLE
      Install-FakeprintIppQueue
      Install-FakeprintIppQueue -Name bpl0 -Force
    #>
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [string]$Name      = 'default',
        [string]$BaseUrl   = $script:DefaultBaseUrl,
        [string]$PrinterName,
        [switch]$Force,
        [switch]$WsdAdd
    )

    if ($WsdAdd) {
        Write-Host 'install-ipp: -WsdAdd requested -- opening Settings -> Printers & scanners'
        Open-FakeprintAddDevice
        return
    }

    if (-not (Test-FakeprintIsAdmin)) {
        throw @'
Install-FakeprintIppQueue requires an elevated (administrator) session.
Run PowerShell as Administrator, then retry:
  .\scripts\fakeprint.ps1 install-ipp
Alternatively (person-click WSD path):
  .\scripts\fakeprint.ps1 add-device
'@
    }

    $BaseUrl = Resolve-FakeprintBaseUrl $BaseUrl

    # --- Resolve advertise host from running instance ---
    $status = $null
    try {
        $status = Invoke-RestMethod -Uri "$BaseUrl/v1/status" -TimeoutSec 5
    } catch {
        throw "Cannot reach fakeprint at $BaseUrl (run 'fakeprint.ps1 ensure' first). Error: $_"
    }

    # device_endpoint = "http://192.168.1.70:3921/WebServices/Device"
    $ippUrl = $null
    if (-not [string]::IsNullOrWhiteSpace([string]$status.device_endpoint)) {
        try {
            $devUri = [Uri][string]$status.device_endpoint
            $ippUrl = "http://$($devUri.Host):$($devUri.Port)/ipp/print"
        } catch {}
    }
    if ([string]::IsNullOrWhiteSpace($ippUrl)) {
        # Fallback: derive from BaseUrl
        try {
            $bu = [Uri]$BaseUrl
            $ippUrl = "http://$($bu.Host):$($bu.Port)/ipp/print"
        } catch {
            $ippUrl = 'http://127.0.0.1:3921/ipp/print'
        }
    }

    $label = if (-not [string]::IsNullOrWhiteSpace($PrinterName)) {
        $PrinterName
    } elseif (-not [string]::IsNullOrWhiteSpace([string]$status.label)) {
        [string]$status.label
    } else {
        'Fake WSD printer'
    }

    $legacyTcpmonPort = "Fakeprint-IPP-$Name"
    $driverName = 'Microsoft IPP Class Driver'

    Write-Host "IPP install: IppURL='$ippUrl'  printer='$label'"

    # Tear down a prior bad TCPMON install (http URL stuffed into RAW :9100) or a
    # JetDirect fall-back queue (Fakeprint-JD-*).  Either way the label name is
    # occupied and would block the -IppURL rename; clear it before creating the
    # proper Internet Port so the label lands on the IPP queue, not the JD queue.
    $legacyPort = Get-PrinterPort -Name $legacyTcpmonPort -ErrorAction SilentlyContinue
    $existingPrinter = Get-Printer -Name $label -ErrorAction SilentlyContinue
    $existingIsJd = $existingPrinter -and $existingPrinter.PortName -like 'Fakeprint-JD-*'
    if ($Force -or $legacyPort -or
        ($existingPrinter -and $existingPrinter.PortName -eq $legacyTcpmonPort) -or
        $existingIsJd) {
        $reason = if ($existingIsJd) { 'JetDirect fall-back queue' } else { 'legacy TCPMON or -Force' }
        Write-Host "  Removing prior queue/port ($reason) to clear label for Internet Port ..."
        if ($PSCmdlet.ShouldProcess($label, 'Remove-Printer')) {
            Get-PrintJob -PrinterName $label -ErrorAction SilentlyContinue | ForEach-Object {
                Remove-PrintJob -PrinterName $label -ID $_.Id -ErrorAction SilentlyContinue
            }
            Remove-Printer -Name $label -ErrorAction SilentlyContinue
        }
        if ($PSCmdlet.ShouldProcess($legacyTcpmonPort, 'Remove-PrinterPort')) {
            Remove-PrinterPort -Name $legacyTcpmonPort -ErrorAction SilentlyContinue
        }
        # Also remove an Internet Port whose name is the IPP URL (re-Add cleanly).
        if ($PSCmdlet.ShouldProcess($ippUrl, 'Remove-PrinterPort')) {
            Remove-PrinterPort -Name $ippUrl -ErrorAction SilentlyContinue
        }
        $existingPrinter = Get-Printer -Name $label -ErrorAction SilentlyContinue
    }

    if ($existingPrinter) {
        $mon = (Get-PrinterPort -Name $existingPrinter.PortName -ErrorAction SilentlyContinue).PortMonitor
        Write-Host "  Printer '$label' already exists on port='$($existingPrinter.PortName)' monitor='$mon'"
        if ($mon -eq 'TCPMON.DLL') {
            Write-Warning "Port is TCPMON RAW (not IPP). Re-run with -Force to recreate via -IppURL."
            return
        }
    } else {
        $added = $false
        Write-Host "  Try Add-Printer -IppURL '$ippUrl' (Internet Port) ..."
        # Pass -Name explicitly so Windows uses the label we want.
        # fakeprint (>=0.1, with separate --ipp-uuid) advertises a distinct printer-uuid
        # in IPP GPA, so this no longer collides with the WSD SWD\DAFWSDProvider device.
        # If the name already exists (JD queue still present) Windows appends a suffix;
        # we detect and rename it below.
        if ($PSCmdlet.ShouldProcess($ippUrl, 'Add-Printer -IppURL')) {
            try {
                Add-Printer -IppURL $ippUrl -Name $label -ErrorAction Stop
                $added = $true
            } catch {
                Write-Warning "Add-Printer -IppURL failed: $_"
            }
        }
        # Second IppURL attempt without explicit name (Windows picks a unique name).
        if (-not $added -and $PSCmdlet.ShouldProcess($ippUrl, 'Add-Printer -IppURL (auto-name)')) {
            try {
                Add-Printer -IppURL $ippUrl -ErrorAction Stop
                $added = $true
            } catch {
                Write-Warning "Add-Printer -IppURL (auto-name) failed: $_"
            }
        }
        if (-not $added -and $PSCmdlet.ShouldProcess($ippUrl, 'Add-Printer -PortName URL')) {
            try {
                Add-Printer -Name $label -DriverName $driverName -PortName $ippUrl -ErrorAction Stop
                $added = $true
            } catch {
                Write-Warning "Add-Printer -PortName <url> failed: $_"
            }
        }
        if (-not $added) {
            # Lab fall-back: TCPMON RAW :9100 against the advertise host.
            # fakeprint's JetDirect listener captures the stream to inbox.
            # Use a distinct port name so a stuck Fakeprint-IPP-* (http:// URL) does not block.
            $hostOnly = ([Uri]$ippUrl).Host
            $portName = "Fakeprint-JD-$Name"
            Write-Host "  Fall-back: TCPMON RAW ${hostOnly}:9100 as '$portName' (JetDirect listener)"
            if ($PSCmdlet.ShouldProcess($portName, 'Add-PrinterPort JetDirect')) {
                $oldJd = Get-PrinterPort -Name $portName -ErrorAction SilentlyContinue
                if ($oldJd) {
                    Remove-PrinterPort -Name $portName -ErrorAction SilentlyContinue
                    Start-Sleep -Milliseconds 500
                }
                Add-PrinterPort -Name $portName -PrinterHostAddress $hostOnly -PortNumber 9100 -ErrorAction Stop
                Add-Printer -Name $label -DriverName $driverName -PortName $portName -ErrorAction Stop
                $added = $true
            }
        }
        if (-not $added) {
            Write-Warning ('IPP queue install failed. Fall-back (person-click WSD path):' +
                "`n  .\scripts\fakeprint.ps1 add-device")
            return
        }

        # Prefer a stable display name for lab scripts.
        #
        # Add-Printer -IppURL creates an Internet Port (WSD Port Monitor) with a
        # WSD-{guid} port name — never the IPP URL string.  Search for it by
        # port-monitor type, skipping any printer already on a known non-fakeprint
        # WSD port (e.g. the Brother WSD port that stays in WSD-43c6…).
        # Fallback to TCPMON legacy port or URL port name for older installs.
        $knownNonFakeWsdPorts = @(Get-Printer -ErrorAction SilentlyContinue |
            Where-Object { $_.PortName -like 'WSD-*' -and $_.Name -notlike '*Fake*' -and $_.Name -notmatch '(?i)fakeprint' } |
            ForEach-Object { $_.PortName })

        $created = Get-Printer -ErrorAction SilentlyContinue | Where-Object {
            $_.PortName -eq $ippUrl -or
            $_.PortName -eq $legacyTcpmonPort -or
            ($_.PortName -like 'WSD-*' -and
             $_.PortName -notin $knownNonFakeWsdPorts -and
             (Get-PrinterPort -Name $_.PortName -ErrorAction SilentlyContinue).PortMonitor -eq 'WSD Port Monitor')
        } | Select-Object -First 1

        if ($created -and $created.Name -ne $label) {
            Write-Host "  Rename-Printer '$($created.Name)' -> '$label'"
            try { Rename-Printer -Name $created.Name -NewName $label -ErrorAction Stop } catch {
                # Name collision: keep the auto-generated name and update $label.
                Write-Warning "Rename-Printer failed (using auto-name '$($created.Name)'): $_"
                $label = $created.Name
            }
        }
    }

    $final = Get-Printer -Name $label -ErrorAction SilentlyContinue
    if (-not $final) {
        # Also check by Internet Port (WSD Port Monitor) in case rename failed or name differs.
        $knownNonFakeWsdPorts = @(Get-Printer -ErrorAction SilentlyContinue |
            Where-Object { $_.PortName -like 'WSD-*' -and $_.Name -notlike '*Fake*' -and $_.Name -notmatch '(?i)fakeprint' } |
            ForEach-Object { $_.PortName })
        $final = Get-Printer -ErrorAction SilentlyContinue | Where-Object {
            $_.PortName -eq $ippUrl -or
            ($_.PortName -like 'WSD-*' -and
             $_.PortName -notin $knownNonFakeWsdPorts -and
             (Get-PrinterPort -Name $_.PortName -ErrorAction SilentlyContinue).PortMonitor -eq 'WSD Port Monitor')
        } | Select-Object -First 1
    }
    if ($final) {
        $mon = (Get-PrinterPort -Name $final.PortName -ErrorAction SilentlyContinue).PortMonitor
        Write-Host "  queue='$($final.Name)' port='$($final.PortName)' monitor='$mon'"
        if ($mon -eq 'TCPMON.DLL') {
            Write-Warning 'Install produced TCPMON again - jobs will stick. Prefer Add-Printer -IppURL.'
        }
    }

    # --- Mark added in registry ---
    Set-FakeprintDeviceAdded -Name $Name -Pnp $true
    Write-Host "  marked added.pnp=true in registry"
    Write-Host ''
    Write-Host "IPP queue ready. Send a test job:"
    Write-Host "  Get-Printer -Name '$label' | Format-List Name,DriverName,PortName"
    Write-Host "  Invoke-RestMethod $BaseUrl/v1/jobs"
}

function Remove-FakeprintIppQueue {
    <#
    .SYNOPSIS
      Remove the IPP spooler queue and port created by Install-FakeprintIppQueue.
    .DESCRIPTION
      Removes the printer named by -PrinterName (default: profile label), any
      legacy TCPMON port Fakeprint-IPP-<Name>, and the Internet Port whose name
      is the IPP URL. Marks added.pnp=false. Does NOT stop the fakeprint process.
    .PARAMETER Name
      Registry row name (default: 'default').
    .PARAMETER BaseUrl
      fakeprint control URL (used to resolve the default printer name from /v1/status).
    .PARAMETER PrinterName
      Explicit printer name; overrides the label from /v1/status.
    .EXAMPLE
      Remove-FakeprintIppQueue
      Remove-FakeprintIppQueue -Name bpl0
    #>
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [string]$Name      = 'default',
        [string]$BaseUrl   = $script:DefaultBaseUrl,
        [string]$PrinterName
    )
    $legacyPort = "Fakeprint-IPP-$Name"
    $jdPort = "Fakeprint-JD-$Name"
    $ippUrl = $null

    # Resolve printer name + IPP URL
    $label = $PrinterName
    try {
        $st = Invoke-RestMethod -Uri "$(Resolve-FakeprintBaseUrl $BaseUrl)/v1/status" -TimeoutSec 3
        if ([string]::IsNullOrWhiteSpace($label) -and -not [string]::IsNullOrWhiteSpace([string]$st.label)) {
            $label = [string]$st.label
        }
        if (-not [string]::IsNullOrWhiteSpace([string]$st.device_endpoint)) {
            $u = [Uri][string]$st.device_endpoint
            $ippUrl = "http://$($u.Host):$($u.Port)/ipp/print"
        }
    } catch {}
    if ([string]::IsNullOrWhiteSpace($label)) { $label = 'Fake WSD printer' }

    $removed = $false
    $p = Get-Printer -Name $label -ErrorAction SilentlyContinue
    if (-not $p) {
        $p = Get-Printer -ErrorAction SilentlyContinue | Where-Object {
            $_.PortName -eq $ippUrl -or $_.PortName -eq $jdPort -or $_.PortName -eq $legacyPort
        } | Select-Object -First 1
        if ($p) { $label = $p.Name }
    }
    if ($p) {
        # Capture the Internet Port name before removing the printer.
        $internetPortName = if ($p.PortName -like 'WSD-*') { $p.PortName } else { $null }
        Write-Host "Removing printer '$label'"
        if ($PSCmdlet.ShouldProcess($label, 'Remove-Printer')) {
            Get-PrintJob -PrinterName $label -ErrorAction SilentlyContinue | ForEach-Object {
                Remove-PrintJob -PrinterName $label -ID $_.Id -ErrorAction SilentlyContinue
            }
            Remove-Printer -Name $label -ErrorAction SilentlyContinue
            $removed = $true
        }
        # Remove the Internet Port (WSD Port Monitor) left behind by Add-Printer -IppURL.
        if ($internetPortName -and $PSCmdlet.ShouldProcess($internetPortName, 'Remove-PrinterPort (Internet Port)')) {
            $iport = Get-PrinterPort -Name $internetPortName -ErrorAction SilentlyContinue
            if ($iport) {
                Write-Host "Removing Internet Port '$internetPortName' (monitor=$($iport.PortMonitor))"
                Remove-PrinterPort -Name $internetPortName -ErrorAction SilentlyContinue
                $removed = $true
            }
        }
    } else {
        Write-Host "Printer '$label' not found (already removed?)"
    }

    foreach ($portName in @($legacyPort, $jdPort, $ippUrl) | Where-Object { $_ }) {
        $port = Get-PrinterPort -Name $portName -ErrorAction SilentlyContinue
        if ($port) {
            Write-Host "Removing port '$portName' (monitor=$($port.PortMonitor))"
            if ($PSCmdlet.ShouldProcess($portName, 'Remove-PrinterPort')) {
                Remove-PrinterPort -Name $portName -ErrorAction SilentlyContinue
                $removed = $true
            }
        }
    }

    Set-FakeprintDeviceAdded -Name $Name -Pnp $false
    if ($removed) { Write-Host "IPP queue removed; added.pnp=false" }
}

# ---------------------------------------------------------------------------
# Ensure: start fakeprint if not already running on the named row's port.
# ---------------------------------------------------------------------------

function Start-FakeprintInstance {
    <#
    .SYNOPSIS
      Start a fakeprint process from a registry row (non-interactive; background).
      Use this only when the process is confirmed not running.
    #>
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [Parameter(Mandatory)][psobject]$Row,
        [string]$RepoRoot
    )
    if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
        $RepoRoot = Split-Path -Parent $PSScriptRoot
    }
    # Prefer row.exe; fall back to cargo run.
    $exe = if (-not [string]::IsNullOrWhiteSpace([string]$Row.exe) -and
                (Test-Path -LiteralPath $Row.exe)) {
        $Row.exe
    } else {
        $null
    }
    $listen    = [string]$Row.listen
    $profile   = [string]$Row.profile
    $uuid      = [string]$Row.device_uuid
    $labName   = [string]$Row.name
    $adv       = [string]$Row.advertise

    if ($exe) {
        $startArgs = @('--listen', $listen, '--profile', $profile,
                       '--device-uuid', $uuid, '--lab-name', $labName)
        if (-not [string]::IsNullOrWhiteSpace($adv)) { $startArgs += '--advertise', $adv }
        if ($PSCmdlet.ShouldProcess($exe, "Start-Process fakeprint")) {
            Start-Process -FilePath $exe -ArgumentList $startArgs -WindowStyle Hidden
        }
    } else {
        # cargo run -- slower but works in a dev checkout.
        $cargoArgs = @('run', '-p', 'fakeprint', '--',
                       '--listen', $listen, '--profile', $profile,
                       '--device-uuid', $uuid, '--lab-name', $labName,
                       '--profiles', (Join-Path $RepoRoot 'profiles'),
                       '--inbox', (Join-Path $RepoRoot 'inbox'))
        if (-not [string]::IsNullOrWhiteSpace($adv)) { $cargoArgs += '--advertise', $adv }
        if ($PSCmdlet.ShouldProcess('cargo', 'cargo run fakeprint')) {
            Start-Process -FilePath 'cargo' -ArgumentList $cargoArgs `
                -WorkingDirectory $RepoRoot -WindowStyle Hidden
        }
    }
}

function Invoke-FakeprintEnsure {
    <#
    .SYNOPSIS
      Ensure a fakeprint instance is running. Attaches if already up; starts if not.
      When added.pnp=false and running as admin, installs the IPP spooler queue
      non-interactively (Add-PrinterPort + Add-Printer with Microsoft IPP Class Driver).
    .DESCRIPTION
      Does NOT kill fakescan or any other product's processes.
      Port band: fakeprint uses odd ports (3921, 3923, ...).

      IPP install path (admin session):
        If the process is up but the spooler queue is not yet installed (added.pnp=false),
        Invoke-FakeprintEnsure calls Install-FakeprintIppQueue automatically.
        On success it marks added.pnp=true. On failure it prints the fall-back steps.

      Fall-back (-WsdAdd or non-admin):
        Prints instructions for Open-FakeprintAddDevice (person-click WSD path).
        Another agent or the person can then add the printer via Settings UI.
    .EXAMPLE
      Invoke-FakeprintEnsure                          # default instance; IPP auto-install if admin
      Invoke-FakeprintEnsure -Name bpl0
      Invoke-FakeprintEnsure -All
      Invoke-FakeprintEnsure -WsdAdd                 # skip IPP; open Settings instead
      Invoke-FakeprintEnsure -Quiet                  # banner only if Add still needed
    #>
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [string]$Name = 'default',
        [switch]$All,
        [string]$RepoRoot,
        [int]$WaitSeconds = 8,
        # Skip IPP queue auto-install; open Settings -> Printers & scanners for
        # the person-click WSD path (fires install-attention).
        [switch]$WsdAdd,
        [switch]$Loud,
        [switch]$Quiet
    )
    if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
        $RepoRoot = Split-Path -Parent $PSScriptRoot
    }
    $reg = Get-FakeprintRegistry
    # Force array so .Count works under Set-StrictMode -Version Latest
    # (a single-item pipeline can unwrap when returned from if/else).
    [object[]]$rows = @()
    if ($All) {
        [object[]]$rows = @($reg.devices)
    } else {
        [object[]]$rows = @($reg.devices | Where-Object { $_.name -eq $Name })
    }
    if ($rows.Count -eq 0) {
        Write-Warning "No device '$Name' in fakeprint registry. Run: fakeprint.ps1 add -Name $Name"
        return
    }
    foreach ($row in $rows) {
        $port = [string]$row.listen -replace '^.*:', ''
        $baseUrl = "http://127.0.0.1:$port"
        $running = $false
        try {
            $null = Invoke-RestMethod -Uri "$baseUrl/health" -TimeoutSec 3
            $running = $true
        } catch {}

        if ($running) {
            Write-Host "fakeprint '$($row.name)' already running at $baseUrl (attach; not restarted)"
            Invoke-RestMethod -Uri "$baseUrl/v1/status" -TimeoutSec 3 | ConvertTo-Json -Depth 3
        } else {
            Write-Host "Starting fakeprint '$($row.name)' on $baseUrl ..."
            Start-FakeprintInstance -Row $row -RepoRoot $RepoRoot
            $deadline = (Get-Date).AddSeconds($WaitSeconds)
            $ok = $false
            while ((Get-Date) -lt $deadline) {
                Start-Sleep -Milliseconds 500
                try {
                    $null = Invoke-RestMethod -Uri "$baseUrl/health" -TimeoutSec 2
                    $ok = $true; break
                } catch {}
            }
            if ($ok) {
                Write-Host "  up -- $baseUrl/v1/status"
            } else {
                Write-Warning "  fakeprint '$($row.name)' did not respond in ${WaitSeconds}s. Check manually."
                continue
            }
        }

        # ----------------------------------------------------------------
        # Auto-install IPP spooler queue when added.pnp = false.
        # Requires admin rights; falls back to WSD Add-device + attention.
        # ----------------------------------------------------------------
        $pnpAdded = $false
        try { $pnpAdded = [bool]$row.added.pnp } catch {}

        if ($pnpAdded) { continue }

        # Already have a Fake / IPP queue from a prior install → bookkeeping only.
        $existing = @(Get-FakeprintPrinters)
        $ippPort = "Fakeprint-IPP-$($row.name)"
        $ippPrinter = Get-Printer -ErrorAction SilentlyContinue | Where-Object {
            $_.PortName -eq $ippPort -or ($_.DriverName -eq 'Microsoft IPP Class Driver' -and $_.Name -match 'Fake')
        }
        if ($existing.Count -gt 0 -or $ippPrinter) {
            Write-Host "ensure: spooler queue already present - marking added.pnp=true"
            Set-FakeprintDeviceAdded -Name ([string]$row.name) -Pnp $true | Out-Null
            continue
        }

        if ($WsdAdd) {
            Write-Host ''
            Write-Host 'ensure: -WsdAdd requested -- opening Settings -> Printers & scanners'
            Open-FakeprintAddDevice -Loud:$Loud -Quiet:$Quiet
        } elseif (Test-FakeprintIsAdmin) {
            Write-Host ''
            Write-Host 'ensure: added.pnp=false -- attempting non-interactive IPP queue install ...'
            try {
                Install-FakeprintIppQueue -Name ([string]$row.name) -BaseUrl $baseUrl
            } catch {
                Write-Warning "ensure: IPP queue install failed: $_"
                Write-Warning 'Fall-back: person-click WSD path (attention follows).'
                Open-FakeprintAddDevice -Loud:$Loud -Quiet:$Quiet
            }
        } else {
            Write-Host ''
            Write-Host 'ensure: added.pnp=false - launching elevated IPP install (UAC) ...'
            $elev = Start-FakeprintElevatedInstallIpp -Name ([string]$row.name) -BaseUrl $baseUrl
            if ($elev -and $elev.Ok) {
                Write-Host 'ensure: elevated IPP install succeeded'
                continue
            }
            Write-Warning 'ensure: elevated IPP install did not finish (UAC denied or queue missing).'
            Write-Host '  Fall-back: person-click WSD path (attention follows).'
            Open-FakeprintAddDevice -Loud:$Loud -Quiet:$Quiet
        }
    }
}

# ---------------------------------------------------------------------------
# Ctl: POST /v1/ctl
# ---------------------------------------------------------------------------

function Invoke-FakeprintCtl {
    <#
    .SYNOPSIS
      POST /v1/ctl {"cmd":"...","args":{...}} to a running fakeprint instance.
    .EXAMPLE
      Invoke-FakeprintCtl status
      Invoke-FakeprintCtl profile -Args @{id='generic-wsd'}
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory, Position = 0)][string]$Cmd,
        [hashtable]$Args = @{},
        [string]$BaseUrl = $script:DefaultBaseUrl
    )
    $BaseUrl = Resolve-FakeprintBaseUrl $BaseUrl
    $body = [ordered]@{ cmd = $Cmd; args = $Args } | ConvertTo-Json -Compress
    Invoke-RestMethod -Method Post -Uri "$BaseUrl/v1/ctl" `
        -ContentType 'application/json' -Body $body -TimeoutSec 10
}

# ---------------------------------------------------------------------------
# Watch-FakeprintWsdAdd
# Poll /v1/status while the person clicks Add device; surface each milestone
# (Subscribe, GetPrinterElements, subscribe dump, PRINTENUM, WSD queue).
# Safe to run against a live server -- never restarts fakeprint.
# ---------------------------------------------------------------------------

function Get-FakeprintTestPagePath {
    <#
    .SYNOPSIS
      Path to python/fixtures/testpage.pdf (visible Letter proof page).
      Regenerates via Python when -Rebuild is set or the file is missing.
    #>
    [CmdletBinding()]
    param([switch]$Rebuild)
    $repo = Split-Path -Parent $PSScriptRoot
    $pdf = Join-Path $repo 'python\fixtures\testpage.pdf'
    $need = $Rebuild -or -not (Test-Path -LiteralPath $pdf)
    if ($need) {
        $env:PYTHONPATH = Join-Path $repo 'python'
        # Capture stdout so it is not part of this function's return pipeline.
        $null = & python -c "from fakeprint.testpage import write_fixture; write_fixture()"
        if ($LASTEXITCODE -ne 0) {
            throw "failed to build testpage.pdf (pip install -e ./python ?)"
        }
    }
    if (-not (Test-Path -LiteralPath $pdf)) {
        throw "testpage.pdf missing at $pdf"
    }
    return (Resolve-Path -LiteralPath $pdf).Path
}

function Send-FakeprintTestPage {
    <#
    .SYNOPSIS
      Print the visible lab test page to a fakeprint spooler queue (PrintTo).

    .DESCRIPTION
      Uses fixtures/testpage.pdf (title, FAKEPRINT-TESTPAGE-v1 marker, gray
      wedge, RGB bars, corner marks) -- not a blank page. Polls GET /v1/jobs
      until a new capture appears. Spooler re-encoding may drop the magic
      string; a non-tiny capture size is still proof the queue delivered.

    .PARAMETER PrinterName
      Spooler queue name. Default: first Get-FakeprintPrinters row (prefers
      WSD / Internet Port over JetDirect when several match).
    #>
    [CmdletBinding(SupportsShouldProcess = $true)]
    param(
        [string]$BaseUrl = $script:DefaultBaseUrl,
        [string]$PrinterName,
        [string]$PdfPath,
        [int]$WaitSeconds = 90,
        [switch]$Rebuild,
        [switch]$Open
    )
    $BaseUrl = Resolve-FakeprintBaseUrl $BaseUrl
    if ([string]::IsNullOrWhiteSpace($PdfPath)) {
        $PdfPath = Get-FakeprintTestPagePath -Rebuild:$Rebuild
    } elseif (-not (Test-Path -LiteralPath $PdfPath)) {
        throw "PdfPath not found: $PdfPath"
    }

    $printers = @(Get-FakeprintPrinters)
    if ($printers.Count -eq 0) {
        throw 'No fakeprint spooler queue found. Run: .\scripts\fakeprint.ps1 install-ipp  or ensure -WsdAdd'
    }
    $chosen = $null
    if (-not [string]::IsNullOrWhiteSpace($PrinterName)) {
        $chosen = $printers | Where-Object { $_.Name -eq $PrinterName } | Select-Object -First 1
        if (-not $chosen) {
            throw "PrinterName '$PrinterName' is not a fakeprint queue. Have: $($printers.Name -join ', ')"
        }
    } else {
        # Prefer WSD / Internet Port (peer=ipp) over JetDirect TCPMON.
        $chosen = $printers |
            Sort-Object {
                $p = [string]$_.PortName
                if ($p -match '(?i)^WSD-') { 0 }
                elseif ($p -match '(?i)IPP|Internet|http') { 1 }
                else { 2 }
            } |
            Select-Object -First 1
    }

    function Get-FakeprintJobRows([string]$Url) {
        $raw = Invoke-RestMethod -Uri "$Url/v1/jobs" -TimeoutSec 5
        if ($null -eq $raw) { return @() }
        if ($raw.PSObject.Properties.Name -contains 'jobs') {
            return @($raw.jobs)
        }
        return @($raw)
    }

    $beforeJobs = @()
    try {
        $beforeJobs = @(Get-FakeprintJobRows $BaseUrl)
    } catch {
        throw ("fakeprint not reachable at {0}/v1/jobs -- is the lab process up? {1}" -f $BaseUrl, $_)
    }
    $beforeIds = @($beforeJobs | ForEach-Object { [int]$_.job_id })
    $beforeMax = if ($beforeIds.Count -gt 0) { ($beforeIds | Measure-Object -Maximum).Maximum } else { 0 }

    $pdfBytes = (Get-Item -LiteralPath $PdfPath).Length
    Write-Host ("PrintTo '{0}' port={1} pdf={2} size={3}" -f `
        $chosen.Name, $chosen.PortName, $PdfPath, $pdfBytes)

    if (-not $PSCmdlet.ShouldProcess($chosen.Name, ("PrintTo {0}" -f $PdfPath))) {
        return [pscustomobject]@{
            Ok = $false
            Skipped = $true
            PrinterName = $chosen.Name
            PdfPath = $PdfPath
        }
    }

    # PrintTo uses the registered PDF handler; -Wait returns when the handler
    # exits, which may be before the spooler finishes -- we poll /v1/jobs.
    $p = Start-Process -FilePath $PdfPath -Verb PrintTo -ArgumentList $chosen.Name -PassThru -WindowStyle Hidden
    if ($p) {
        try { Wait-Process -Id $p.Id -Timeout 60 -ErrorAction SilentlyContinue } catch { }
    }

    $deadline = [datetime]::UtcNow.AddSeconds($WaitSeconds)
    $newJob = $null
    while ([datetime]::UtcNow -lt $deadline) {
        Start-Sleep -Seconds 2
        try {
            $jobs = @(Get-FakeprintJobRows $BaseUrl)
        } catch {
            Write-Warning "jobs poll failed: $_"
            continue
        }
        $fresh = @($jobs | Where-Object { [int]$_.job_id -gt $beforeMax })
        if ($fresh.Count -gt 0) {
            $newJob = $fresh | Sort-Object { [int]$_.job_id } | Select-Object -Last 1
            break
        }
    }

    if (-not $newJob) {
        return [pscustomobject]@{
            Ok = $false
            PrinterName = $chosen.Name
            PortName = $chosen.PortName
            PdfPath = $PdfPath
            PdfBytes = $pdfBytes
            Error = "no new /v1/jobs capture within ${WaitSeconds}s"
        }
    }

    $capPath = [string]$newJob.path
    $capBytes = [int]$newJob.bytes
    $fullPath = $null
    try {
        $st = Invoke-RestMethod -Uri "$BaseUrl/v1/status" -TimeoutSec 5
        $inbox = [string]$st.inbox_dir
        if ($inbox -and $capPath) {
            $fullPath = if ([IO.Path]::IsPathRooted($capPath)) { $capPath } else { Join-Path $inbox $capPath }
        }
    } catch {
        Write-Warning "could not resolve capture path: $_"
    }

    $proof = $null
    if ($fullPath -and (Test-Path -LiteralPath $fullPath)) {
        try {
            $proof = Invoke-FakeprintCaptureProof -Path $fullPath
        } catch {
            Write-Warning "capture proof analyze failed: $_"
        }
    }

    $magic = [bool]($proof -and $proof.Magic)
    $visual = [bool]($proof -and $proof.Visual)
    $substantial = if ($proof) { [bool]$proof.Substantial } else { $capBytes -ge 1024 }
    $ok = if ($proof) { [bool]$proof.Ok } else { $substantial }
    $note = if ($proof) { [string]$proof.Note } else {
        if ($ok) { 'capture size looks non-blank (analyzer unavailable)' }
        else { 'capture still tiny -- likely a blank handler page; check PDF association / PrintTo' }
    }

    $preview = $null
    if ($Open -and $fullPath -and (Test-Path -LiteralPath $fullPath)) {
        try {
            $opened = Open-FakeprintCapture -Path $fullPath -JobId ([int]$newJob.job_id)
            $preview = $opened.Preview
        } catch {
            Write-Warning "Open-FakeprintCapture failed: $_"
        }
    }

    return [pscustomobject]@{
        Ok = [bool]$ok
        PrinterName = $chosen.Name
        PortName = $chosen.PortName
        DriverName = $chosen.DriverName
        PdfPath = $PdfPath
        PdfBytes = $pdfBytes
        JobId = [int]$newJob.job_id
        CaptureBytes = $capBytes
        CapturePath = $fullPath
        DocumentFormat = $newJob.document_format
        Peer = $newJob.peer
        MagicSurvived = $magic
        VisualProof = $visual
        ColorHits = if ($proof) { $proof.ColorHits } else { $null }
        Substantial = $substantial
        Note = $note
        Preview = $preview
    }
}

function Invoke-FakeprintCaptureProof {
    <#
    .SYNOPSIS
      Score an inbox .bin for test-page proof (magic and/or signature RGB fills).
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "capture not found: $Path"
    }
    $repo = Split-Path -Parent $PSScriptRoot
    $env:PYTHONPATH = Join-Path $repo 'python'
    $env:FAKEPRINT_CAPTURE_PATH = $Path
    # Write a tiny runner so native `python -c` does not strip quotes.
    $runner = Join-Path $env:TEMP 'fakeprint-capture-proof.py'
    @(
        'import json, os'
        'from fakeprint.testpage import analyze_capture_file'
        'p = analyze_capture_file(os.environ["FAKEPRINT_CAPTURE_PATH"])'
        'print(json.dumps({'
        '  "magic": p.magic,'
        '  "visual": p.visual,'
        '  "ok": p.ok,'
        '  "color_hits": p.color_hits,'
        '  "colors_expected": p.colors_expected,'
        '  "flate_streams": p.flate_streams,'
        '  "substantial": p.substantial,'
        '  "bytes": p.bytes,'
        '  "note": p.note,'
        '}))'
    ) | Set-Content -LiteralPath $runner -Encoding UTF8
    $json = & python $runner
    Remove-Item -LiteralPath $runner -ErrorAction SilentlyContinue
    if ($LASTEXITCODE -ne 0) {
        throw "analyze_capture_file failed for $Path"
    }
    $o = $json | ConvertFrom-Json
    [pscustomobject]@{
        Magic = [bool]$o.magic
        Visual = [bool]$o.visual
        Ok = [bool]$o.ok
        ColorHits = [int]$o.color_hits
        ColorsExpected = [int]$o.colors_expected
        FlateStreams = [int]$o.flate_streams
        Substantial = [bool]$o.substantial
        Bytes = [int]$o.bytes
        Note = [string]$o.note
        Path = $Path
    }
}

function Open-FakeprintCapture {
    <#
    .SYNOPSIS
      Copy an inbox capture to a .pdf and open it (latest job by default).
    #>
    [CmdletBinding()]
    param(
        [string]$BaseUrl = $script:DefaultBaseUrl,
        [int]$JobId = 0,
        [string]$Path,
        [switch]$NoOpen
    )
    $BaseUrl = Resolve-FakeprintBaseUrl $BaseUrl
    $src = $Path
    if ([string]::IsNullOrWhiteSpace($src)) {
        $rows = @()
        $raw = Invoke-RestMethod -Uri "$BaseUrl/v1/jobs" -TimeoutSec 5
        if ($raw.PSObject.Properties.Name -contains 'jobs') { $rows = @($raw.jobs) } else { $rows = @($raw) }
        if ($JobId -gt 0) {
            $hit = $rows | Where-Object { [int]$_.job_id -eq $JobId } | Select-Object -First 1
        } else {
            $hit = $rows | Sort-Object { [int]$_.job_id } | Select-Object -Last 1
        }
        if (-not $hit) { throw 'no captured jobs to open' }
        $src = [string]$hit.path
        if (-not [IO.Path]::IsPathRooted($src)) {
            $st = Invoke-RestMethod -Uri "$BaseUrl/v1/status" -TimeoutSec 5
            $src = Join-Path ([string]$st.inbox_dir) $src
        }
        $JobId = [int]$hit.job_id
    }
    if (-not (Test-Path -LiteralPath $src)) {
        throw "capture not found: $src"
    }
    $destDir = Join-Path $env:TEMP 'fakeprint-preview'
    New-Item -ItemType Directory -Force -Path $destDir | Out-Null
    $dest = Join-Path $destDir ("job-{0}.pdf" -f $(if ($JobId -gt 0) { $JobId } else { 'x' }))
    Copy-Item -LiteralPath $src -Destination $dest -Force
    $proof = $null
    try { $proof = Invoke-FakeprintCaptureProof -Path $src } catch { }
    if (-not $NoOpen) {
        Start-Process -FilePath $dest
    }
    [pscustomobject]@{
        JobId = $JobId
        Source = $src
        Preview = $dest
        Proof = $proof
    }
}

function Watch-FakeprintWsdAdd {
    <#
    .SYNOPSIS
      Poll /v1/status while a person clicks Add device; report milestones as
      they arrive: Subscribe, GetPrinterElements, PRINTENUM, WSD queue.
    .DESCRIPTION
      Does NOT restart fakeprint or touch the spooler. Safe to call while the
      server is live. Run alongside Open-FakeprintAddDevice / 'fakeprint.ps1
      add-device' or 'fakeprint.ps1 ensure -WsdAdd'.

      Milestones checked each poll:
        1. wsd_actions.Subscribe rises    -- WSDMON called Subscribe
        2. wsd_actions.GetPrinterElements rises -- WSDMON read caps
        3. fakeprint subscribe dump XML   -- NotifyTo address logged
        4. SWD\PRINTENUM\WSD-* device    -- WSD port committed by spooler
        5. Get-Printer WSD port           -- queue ready to print

      Exits on success (milestone 5) or after -TimeoutSec seconds.
    .EXAMPLE
      Watch-FakeprintWsdAdd
      Watch-FakeprintWsdAdd -TimeoutSec 180
    #>
    [CmdletBinding()]
    param(
        [string]$BaseUrl    = $script:DefaultBaseUrl,
        [int]$TimeoutSec    = 120,
        [int]$PollMs        = 2000
    )

    $BaseUrl = Resolve-FakeprintBaseUrl $BaseUrl

    # --- Baseline ----------------------------------------------------------
    $baseline = $null
    try {
        $baseline = Invoke-RestMethod -Uri "$BaseUrl/v1/status" -TimeoutSec 5
    } catch {
        Write-Warning "Watch-FakeprintWsdAdd: cannot reach fakeprint at $BaseUrl"
        return [pscustomobject]@{ Ok = $false; Milestones = $null; Queue = $null }
    }

    # Safe count read for wsd_actions (keys absent when count = 0).
    # PS 5.1 + Set-StrictMode -Version Latest: accessing a missing PSCustomObject
    # property throws; catch and return 0.
    $getCount = {
        param([object]$bag, [string]$key)
        if ($null -eq $bag) { return [int]0 }
        try { return [int]($bag.$key) } catch { return [int]0 }
    }

    $baseSub = & $getCount $baseline.wsd_actions 'Subscribe'
    $baseGpe = & $getCount $baseline.wsd_actions 'GetPrinterElements'

    # Derive the subscribe-dump dir: fakeprint writes fakeprint-subscribe-*.xml
    # to the parent of inbox_dir (e.g. D:\temp when inbox = D:\temp\fakeprint-inbox).
    $dumpDir = ''
    try {
        $inboxStr = [string]$baseline.inbox_dir
        if (-not [string]::IsNullOrWhiteSpace($inboxStr)) {
            $dumpDir = Split-Path -Parent $inboxStr
        }
    } catch {}

    # Baseline dump timestamp so milestone 3 fires only on NEW dumps written
    # after Watch-FakeprintWsdAdd starts (prior session dumps already on disk).
    $baseDumpNewest = [datetime]::MinValue
    if (-not [string]::IsNullOrWhiteSpace($dumpDir)) {
        try {
            $bd = @(Get-ChildItem -Path (Join-Path $dumpDir 'fakeprint-subscribe-*.xml') `
                    -ErrorAction SilentlyContinue | Sort-Object LastWriteTime)
            if ($bd.Count -gt 0) { $baseDumpNewest = $bd[-1].LastWriteTime }
        } catch {}
    }

    # --- Milestone record --------------------------------------------------
    $ms = [ordered]@{
        Subscribe          = $false
        GetPrinterElements = $false
        SubscribeDump      = $false
        Printenum          = $false
        WsdQueue           = $false
    }

    $origFg = $host.UI.RawUI.ForegroundColor
    $writeMilestone = {
        param([string]$msg)
        try { $host.UI.RawUI.ForegroundColor = 'Green' } catch {}
        Write-Host $msg
        try { $host.UI.RawUI.ForegroundColor = $origFg } catch {}
    }

    # --- Header ------------------------------------------------------------
    Write-Host ''
    Write-Host "Watch-FakeprintWsdAdd  $BaseUrl  timeout=${TimeoutSec}s"
    Write-Host "  Baseline: Subscribe=$baseSub  GetPrinterElements=$baseGpe"
    Write-Host '  Waiting for:'
    Write-Host '    1. Subscribe rises              WSDMON called Subscribe'
    Write-Host '    2. GetPrinterElements rises      WSDMON read caps'
    Write-Host '    3. Subscribe dump XML            NotifyTo address logged'
    Write-Host '    4. SWD\PRINTENUM\WSD-* device   WSD port committed'
    Write-Host '    5. Get-Printer WSD port          queue ready to print'
    Write-Host ''

    # --- Poll loop ---------------------------------------------------------
    $start    = Get-Date
    $deadline = $start.AddSeconds($TimeoutSec)
    $lastLine = ''

    while ((Get-Date) -lt $deadline) {
        $elapsed = [int]((Get-Date) - $start).TotalSeconds
        Start-Sleep -Milliseconds $PollMs

        $st = $null
        try { $st = Invoke-RestMethod -Uri "$BaseUrl/v1/status" -TimeoutSec 4 } catch { continue }

        $sub = & $getCount $st.wsd_actions 'Subscribe'
        $gpe = & $getCount $st.wsd_actions 'GetPrinterElements'

        # Milestone 1: Subscribe
        if (-not $ms.Subscribe -and $sub -gt $baseSub) {
            $ms.Subscribe = $true
            & $writeMilestone "  [+] Subscribe: $baseSub→$sub  WSDMON called Subscribe (+${elapsed}s)"
        }
        # Milestone 2: GetPrinterElements
        if (-not $ms.GetPrinterElements -and $gpe -gt $baseGpe) {
            $ms.GetPrinterElements = $true
            & $writeMilestone "  [+] GetPrinterElements: $baseGpe→$gpe  WSDMON read caps (+${elapsed}s)"
        }
        # Milestone 3: subscribe dump file (new since watcher started)
        if (-not $ms.SubscribeDump -and -not [string]::IsNullOrWhiteSpace($dumpDir)) {
            $dumps = @(Get-ChildItem -Path (Join-Path $dumpDir 'fakeprint-subscribe-*.xml') `
                        -ErrorAction SilentlyContinue |
                       Where-Object { $_.LastWriteTime -gt $baseDumpNewest } |
                       Sort-Object LastWriteTime)
            if ($dumps.Count -gt 0) {
                $ms.SubscribeDump = $true
                & $writeMilestone "  [+] Subscribe dump: $($dumps[-1].Name)  (+${elapsed}s)"
            }
        }
        # Milestone 4: PRINTENUM device
        if (-not $ms.Printenum) {
            $peOut  = pnputil /enum-devices /class '{4d36e979-e325-11ce-bfc1-08002be10318}' 2>$null
            $peLine = $peOut | Select-String 'WSD-0000' | Select-Object -First 1
            if ($peLine) {
                $ms.Printenum = $true
                & $writeMilestone "  [+] PRINTENUM: $($peLine.Line.Trim())  (+${elapsed}s)"
            }
        }
        # Milestone 5: WSD queue in spooler
        if (-not $ms.WsdQueue) {
            $wq = @(Get-Printer -ErrorAction SilentlyContinue |
                    Where-Object { $_.PortName -like '*WSD*' }) | Select-Object -First 1
            if ($wq) {
                $ms.WsdQueue = $true
                & $writeMilestone "  [+] WSD queue: $($wq.Name)  port=$($wq.PortName)  (+${elapsed}s)"
            }
        }

        if ($ms.WsdQueue) { break }

        $statusLine = "  +${elapsed}s  Sub=$sub  GPE=$gpe  printenum=$($ms.Printenum)  queue=$($ms.WsdQueue)"
        if ($statusLine -ne $lastLine) { Write-Host $statusLine; $lastLine = $statusLine }
    }

    # --- Summary -----------------------------------------------------------
    $ok = [bool]$ms.WsdQueue
    Write-Host ''
    if ($ok) {
        try { $host.UI.RawUI.ForegroundColor = 'Green' } catch {}
        Write-Host 'Watch-FakeprintWsdAdd: SUCCESS — WSD queue installed and visible.'
        try { $host.UI.RawUI.ForegroundColor = $origFg } catch {}
    } else {
        try { $host.UI.RawUI.ForegroundColor = 'Yellow' } catch {}
        Write-Host "Watch-FakeprintWsdAdd: timed out (${TimeoutSec}s)."
        Write-Host "  Milestones: $($ms | ConvertTo-Json -Compress)"
        if (-not $ms.Subscribe) {
            Write-Host '  No Subscribe → WSDMON may have cached state from a prior install.'
            Write-Host '    Try: pnputil /remove-device "SWD\DAFWSDProvider\urn:uuid:..000f3" /subtree'
            Write-Host '         (wait for fakeprint Hello, then Add device again)'
        } elseif (-not $ms.Printenum) {
            Write-Host '  Subscribe rose but no PRINTENUM → NotifyTo unreachable.'
            if (-not [string]::IsNullOrWhiteSpace($dumpDir)) {
                Write-Host "    Review: Get-ChildItem '$dumpDir\fakeprint-subscribe-*.xml' | Select-Object -Last 1"
            }
            Write-Host '    Extract <wse:NotifyTo><wsa:Address> and run: Test-NetConnection <host> -Port <port>'
        }
        try { $host.UI.RawUI.ForegroundColor = $origFg } catch {}
    }

    $wsdQ = @(Get-Printer -ErrorAction SilentlyContinue |
              Where-Object { $_.PortName -like '*WSD*' }) | Select-Object -First 1
    [pscustomobject]@{
        Ok         = $ok
        Milestones = $ms
        Queue      = $wsdQ
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
    'Disconnect-FakeprintAssociationEndpoint',
    'Get-FakeprintDiscoveryReport',
    # Registry
    'Get-FakeprintRegistryPath',
    'Get-FakeprintRegistry',
    'Save-FakeprintRegistry',
    'Get-FakeprintDevices',
    'Add-FakeprintDevice',
    'Remove-FakeprintDevice',
    'Save-FakeprintDevice',
    'Set-FakeprintDeviceAdded',
    # Ensure + ctl
    'Invoke-FakeprintEnsure',
    'Invoke-FakeprintCtl',
    'Start-FakeprintInstance',
    # IPP non-interactive install
    'Test-FakeprintIsAdmin',
    'Start-FakeprintElevatedShell',
    'Start-FakeprintElevatedInstallIpp',
    'Install-FakeprintIppQueue',
    'Remove-FakeprintIppQueue',
    # WSD Add-device verify
    'Watch-FakeprintWsdAdd',
    # Visible test page
    'Get-FakeprintTestPagePath',
    'Send-FakeprintTestPage',
    'Invoke-FakeprintCaptureProof',
    'Open-FakeprintCapture'
)

