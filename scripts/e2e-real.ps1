<#
 Real-hardware end-to-end test of the INSTALLED Lifeboat service, over its named pipe.
 Run it yourself, from an ELEVATED PowerShell:

   powershell -ExecutionPolicy Bypass -File scripts\e2e-real.ps1 -Disk 1 -SizeGb 10 -Confirm
   powershell -ExecutionPolicy Bypass -File scripts\e2e-real.ps1 -Disk 1 -Cleanup -Confirm

 It shrinks the largest lettered volume of -Disk by -SizeGb and creates a hidden Lifeboat
 partition there (PrepareDisk), then runs the whole flow with timings and PASS/FAIL per step.
 It refuses disk 0 and any boot or system disk. -Cleanup removes ONLY the partition this script
 created (its guid is saved in %TEMP%\lifeboat-e2e-state.json) and grows the volume back.

 WARNING: PrepareDisk saves the new partition_guid into C:\ProgramData\Lifeboat\config.json,
 and InitRepo is refused when a repository already exists. If the service already has a
 repository the script stops, unless -AllowExistingConfig is given; then config.json is copied
 to %TEMP% first and put back by -Cleanup.
#>
param(
    [Parameter(Mandatory = $true)][int]$Disk,
    [int]$SizeGb = 10,
    [switch]$Confirm,
    [switch]$Cleanup,
    [switch]$AllowExistingConfig
)
$ErrorActionPreference = 'Stop'
$Pipe = 'lifeboat'
$Password = 'e2e-test-' + [guid]::NewGuid().ToString('N').Substring(0, 12)
$StateFile = Join-Path $env:TEMP 'lifeboat-e2e-state.json'
$LogFile = Join-Path $env:TEMP ("lifeboat-e2e-{0:yyyyMMdd-HHmmss}.log" -f (Get-Date))
$Cfg = Join-Path $env:ProgramData 'Lifeboat\config.json'
$Results = New-Object System.Collections.Generic.List[object]

function Log($m) { $m | Tee-Object -FilePath $LogFile -Append | Write-Host }

function Send([hashtable]$req) {
    $c = New-Object System.IO.Pipes.NamedPipeClientStream('.', $Pipe, [System.IO.Pipes.PipeDirection]::InOut)
    try {
        $c.Connect(10000)
        $enc = New-Object System.Text.UTF8Encoding($false)
        $w = New-Object System.IO.StreamWriter($c, $enc); $w.AutoFlush = $true
        $r = New-Object System.IO.StreamReader($c, $enc)
        $w.WriteLine(($req | ConvertTo-Json -Compress -Depth 8))
        $line = $r.ReadLine()
        if (-not $line) { throw 'empty reply' }
        return ($line | ConvertFrom-Json)
    } finally { $c.Dispose() }
}

function Step([string]$name, [scriptblock]$body) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $ok = $true; $note = ''
    try { $note = [string](& $body) } catch { $ok = $false; $note = $_.Exception.Message }
    $sw.Stop()
    $res = if ($ok) { 'PASS' } else { 'FAIL' }
    $Results.Add([pscustomobject]@{ Step = $name; Result = $res; Ms = [int]$sw.ElapsedMilliseconds; Note = $note })
    Log ("{0,-4} {1,-34} {2,7} ms  {3}" -f $res, $name, [int]$sw.ElapsedMilliseconds, $note)
    return $ok
}

function Expect($resp, [string]$reply) {
    if ($resp.reply -eq 'Error') { throw "service error: $($resp.message)" }
    if ($resp.reply -ne $reply) { throw "expected $reply, got $($resp.reply)" }
}

function Get-TreeHash([string]$root) {
    $base = $root.TrimEnd('\').Length
    Get-ChildItem -LiteralPath $root -Recurse -File | Sort-Object FullName | ForEach-Object {
        '{0}|{1}' -f $_.FullName.Substring($base), (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
    }
}

# ---- safety checks ----
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { throw 'Run this from an elevated (administrator) PowerShell.' }
if (-not $Confirm) { throw "Add -Confirm: this changes the partition layout of disk $Disk." }
if ($Disk -eq 0) { throw 'Refusing disk 0.' }
$d = Get-Disk -Number $Disk -ErrorAction Stop
if ($d.IsBoot -or $d.IsSystem) { throw "Refusing disk $Disk (boot/system disk)." }
Log "Lifeboat e2e - disk $Disk ($($d.FriendlyName), $([math]::Round($d.Size/1GB)) GB), log: $LogFile"

# ---- cleanup mode ----
if ($Cleanup) {
    if (-not (Test-Path $StateFile)) { throw "No saved state ($StateFile): nothing this script created." }
    $s = Get-Content $StateFile -Raw | ConvertFrom-Json
    if ([int]$s.Disk -ne $Disk) { throw "State says disk $($s.Disk), you passed $Disk." }
    $p = $null
    if ($s.PartitionGuid) { $p = Get-Partition -DiskNumber $Disk | Where-Object { $_.Guid -eq $s.PartitionGuid } }
    Step 'Cleanup: stop service' { Stop-Service lifeboat-svc -ErrorAction SilentlyContinue; 'stopped' } | Out-Null
    if ($p) {
        Step 'Cleanup: remove test partition' {
            Remove-Partition -DiskNumber $Disk -PartitionNumber $p.PartitionNumber -Confirm:$false
            'removed partition ' + $p.PartitionNumber
        } | Out-Null
    } else { Log 'Test partition not found (already gone, or never created).' }
    Step 'Cleanup: extend volume back' {
        $dl = [char]$s.DriveLetter
        $max = (Get-PartitionSupportedSize -DriveLetter $dl).SizeMax
        Resize-Partition -DriveLetter $dl -Size $max
        "extended $($s.DriveLetter): to $([math]::Round($max/1GB,1)) GB"
    } | Out-Null
    Step 'Cleanup: restore service config' {
        if ($s.ConfigBackup -and (Test-Path $s.ConfigBackup)) { Copy-Item $s.ConfigBackup $Cfg -Force; 'config restored' }
        elseif (Test-Path $Cfg) { Remove-Item $Cfg -Force; 'test config removed' }
        else { 'nothing to restore' }
    } | Out-Null
    Start-Service lifeboat-svc -ErrorAction SilentlyContinue
    Remove-Item $StateFile -Force -ErrorAction SilentlyContinue
    if ($s.WorkDir -and (Test-Path $s.WorkDir)) { Remove-Item $s.WorkDir -Recurse -Force -ErrorAction SilentlyContinue }
    Log ($Results | Format-Table Step, Result, Ms, Note -AutoSize -Wrap | Out-String -Width 200)
    return
}

if (Test-Path $StateFile) { throw "A previous test left $StateFile. Run with -Cleanup first." }
$existing = $false
if (Test-Path $Cfg) { try { $existing = [bool]((Get-Content $Cfg -Raw | ConvertFrom-Json).repo) } catch { $existing = $true } }
if ($existing -and -not $AllowExistingConfig) {
    throw 'The service already has a repository configured; the test would replace it. Re-run with -AllowExistingConfig to back it up and restore it on -Cleanup.'
}

$work = Join-Path $env:TEMP ('lifeboat-e2e-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$src = Join-Path $work 'src'; $restoreDir = Join-Path $work 'restore'
$vaultSrc = Join-Path $work 'vaultsrc'; $extractDir = Join-Path $work 'extract'
New-Item -ItemType Directory -Force -Path $src, $vaultSrc | Out-Null
$cfgBackup = $null
if ($existing) { $cfgBackup = Join-Path $env:TEMP 'lifeboat-e2e-config-backup.json'; Copy-Item $Cfg $cfgBackup -Force }

# ~200 generated files, some nested
$rnd = New-Object System.Random 42
foreach ($i in 1..200) {
    $dir = $src
    if ($i % 4 -eq 0) { $dir = Join-Path $src ("sub$($i % 7)") }
    if ($i % 10 -eq 0) { $dir = Join-Path $dir ("deep$($i % 3)\inner") }
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $buf = New-Object byte[] (1024 * (1 + $rnd.Next(64))); $rnd.NextBytes($buf)
    [IO.File]::WriteAllBytes((Join-Path $dir "file$i.bin"), $buf)
}
foreach ($i in 1..5) { Set-Content -Path (Join-Path $vaultSrc "secret$i.txt") -Value ("vault file $i " * 50) }
$srcHash = Get-TreeHash $src
$vaultHash = Get-TreeHash $vaultSrc
$srcRoot = (Get-Item $src).FullName
$vaultFiles = @((Get-ChildItem -LiteralPath $vaultSrc -File).FullName)

$script:snap = $null; $script:partGuid = $null; $script:tops = $null; $script:sub = $null
$script:disks = $null; $script:vitems = @()

Step 'Ping' { Expect (Send @{cmd = 'Ping'}) 'Pong'; 'pong' } | Out-Null
Step 'Status' { $r = Send @{cmd = 'Status'}; Expect $r 'Status'; "version $($r.version), frozen=$($r.frozen)" } | Out-Null
Step 'ListDisks' {
    $r = Send @{cmd = 'ListDisks'}; Expect $r 'Disks'; $script:disks = $r.disks
    if (-not ($r.disks | Where-Object { $_.number -eq $Disk })) { throw "disk $Disk not listed" }
    "$(@($r.disks).Count) disks"
} | Out-Null

$ok = Step "PrepareDisk (shrink, $SizeGb GB)" {
    $dk = $script:disks | Where-Object { $_.number -eq $Disk }
    if ($dk.is_boot -or $dk.is_system) { throw 'service reports boot/system disk' }
    $lp = $dk.partitions | Where-Object { $_.drive_letter } | Sort-Object size -Descending | Select-Object -First 1
    if (-not $lp) { throw "no lettered volume on disk $Disk to shrink" }
    $state = @{ Disk = $Disk; DriveLetter = "$($lp.drive_letter)"; PartitionGuid = ''; WorkDir = $work; ConfigBackup = $cfgBackup }
    $state | ConvertTo-Json | Set-Content $StateFile
    $r = Send @{cmd = 'PrepareDisk'; password = $Password; disk = $Disk; mode = @{mode = 'InternalShrink'; drive_letter = "$($lp.drive_letter)"; size_bytes = [uint64]$SizeGb * 1GB}}
    Expect $r 'Prepared'
    $script:partGuid = $r.partition_guid
    $state.PartitionGuid = $r.partition_guid
    $state | ConvertTo-Json | Set-Content $StateFile
    "guid $($r.partition_guid) (saved for -Cleanup)"
}
if (-not $ok) { Log 'PrepareDisk failed; if it changed anything, run with -Cleanup.' }
else {
    $ok2 = Step 'InitRepo (200 files)' {
        $r = Send @{cmd = 'InitRepo'; password = $Password; folders = @($srcRoot)}; Expect $r 'RepoInitialized'; 'recovery key received'
    }
    if ($ok2) {
        Step 'Backup' { $r = Send @{cmd = 'Backup'}; Expect $r 'BackupDone'; $script:snap = $r.snapshot; "snapshot $($r.snapshot)" } | Out-Null
        Step 'ListSnapshots (1st)' {
            $r = Send @{cmd = 'ListSnapshots'}; Expect $r 'Snapshots'
            if (-not @($r.snapshots).Count) { throw 'no snapshots' }
            "$(@($r.snapshots).Count) snapshots"
        } | Out-Null
        Step 'ListSnapshots (2nd, cached)' { $r = Send @{cmd = 'ListSnapshots'}; Expect $r 'Snapshots'; 'ok' } | Out-Null
        Step 'ListFiles root (1st)' {
            $r = Send @{cmd = 'ListFiles'; snapshot = $script:snap}; Expect $r 'Files'
            $script:tops = @($r.entries); "$(@($r.entries).Count) entries"
        } | Out-Null
        Step 'ListFiles root (2nd, cached)' { $r = Send @{cmd = 'ListFiles'; snapshot = $script:snap}; Expect $r 'Files'; 'ok' } | Out-Null
        Step 'ListFiles subfolder (1st)' {
            $t = $script:tops | Where-Object { $_.dir } | Select-Object -First 1
            if (-not $t) { throw 'no folder entry at root' }
            $r = Send @{cmd = 'ListFiles'; snapshot = $script:snap; path = $t.path}; Expect $r 'Files'
            $script:sub = $t
            "$(@($r.entries).Count) entries in $($t.name)"
        } | Out-Null
        Step 'ListFiles subfolder (2nd, cached)' {
            $r = Send @{cmd = 'ListFiles'; snapshot = $script:snap; path = $script:sub.path}; Expect $r 'Files'; 'ok'
        } | Out-Null
        Step 'Restore + hash compare' {
            New-Item -ItemType Directory -Force -Path $restoreDir | Out-Null
            $r = Send @{cmd = 'Restore'; password = $Password; snapshot = $script:snap; target = $restoreDir; paths = @()}; Expect $r 'Restored'
            $f = Get-ChildItem -LiteralPath $restoreDir -Recurse -Filter 'file1.bin' | Select-Object -First 1
            if (-not $f) { throw 'file1.bin not restored' }
            $a = Get-TreeHash $f.DirectoryName
            if (($a -join "`n") -ne ($srcHash -join "`n")) { throw 'restored files differ from the originals' }
            "$(@($a).Count) files identical"
        } | Out-Null
        Step 'VaultAdd' {
            $r = Send @{cmd = 'VaultAdd'; password = $Password; paths = $vaultFiles}; Expect $r 'VaultAdded'
            "moved $($r.moved)"
        } | Out-Null
        Step 'VaultList' {
            $r = Send @{cmd = 'VaultList'}; Expect $r 'Vault'; $script:vitems = @($r.items)
            if ($script:vitems.Count -lt 5) { throw "only $($script:vitems.Count) items" }
            "$($script:vitems.Count) items"
        } | Out-Null
        Step 'VaultExtract + hash compare' {
            New-Item -ItemType Directory -Force -Path $extractDir | Out-Null
            $r = Send @{cmd = 'VaultExtract'; password = $Password; paths = @($script:vitems | ForEach-Object { $_.path }); target = $extractDir}
            Expect $r 'Restored'
            $f = Get-ChildItem -LiteralPath $extractDir -Recurse -Filter 'secret1.txt' | Select-Object -First 1
            if (-not $f) { throw 'secret1.txt not extracted' }
            $a = Get-TreeHash $f.DirectoryName
            if (($a -join "`n") -ne ($vaultHash -join "`n")) { throw 'extracted files differ' }
            "$(@($a).Count) files identical"
        } | Out-Null
        Step 'Status speed (5 calls)' {
            $t = foreach ($i in 1..5) { (Measure-Command { Expect (Send @{cmd = 'Status'}) 'Status' }).TotalMilliseconds }
            'avg {0} ms, max {1} ms' -f [int](($t | Measure-Object -Average).Average), [int](($t | Measure-Object -Maximum).Maximum)
        } | Out-Null
        Log 'Waiting 40 s for the volume lease to expire (30 s idle) ...'
        Start-Sleep -Seconds 40
        Step 'Hidden again after idle' {
            $p = Get-Partition -DiskNumber $Disk | Where-Object { $_.Guid -eq $script:partGuid }
            if (-not $p) { throw 'test partition not found' }
            if ($p.DriveLetter) { throw "drive letter $($p.DriveLetter) is assigned" }
            $attr = [uint64]$p.GptAttributes
            $hid = ($attr -band 0x4000000000000000) -ne 0
            if (-not ($p.IsHidden -or $hid)) { throw "not hidden (IsHidden=$($p.IsHidden), attributes=0x$($attr.ToString('X')))" }
            "IsHidden=$($p.IsHidden), no drive letter, GPT attributes 0x$($attr.ToString('X'))"
        } | Out-Null
    }
}

Log ''
Log 'Summary'
Log ($Results | Format-Table Step, Result, Ms, Note -AutoSize -Wrap | Out-String -Width 200)
$fail = @($Results | Where-Object { $_.Result -eq 'FAIL' }).Count
Log ("{0} steps, {1} failed. Log: {2}" -f $Results.Count, $fail, $LogFile)
Log "When done: scripts\e2e-real.ps1 -Disk $Disk -Cleanup -Confirm"
if ($fail) { exit 1 }
