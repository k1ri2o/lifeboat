# Getting your backups back without Lifeboat

Use this if Lifeboat will not start, or this PC is gone and you only have the backup drive.
You need three things: the drive, a Windows PC, and either your Lifeboat password or your
recovery key (the one you wrote down at setup; restic treats it as just another password).
You also need `restic.exe`: download it from <https://github.com/restic/restic/releases>
(or use the copy bundled with Lifeboat).

Lifeboat keeps backups in a normal restic repository in a folder named `repo`, at the top of
a hidden partition on the drive. Nothing here changes your backups: you only unhide the
partition, read from it, and hide it again.

If you can, just plug the drive into a PC with Lifeboat installed: setup step 1 shows it as
"Existing Lifeboat drive". The steps below are for when that is not possible.

## 1. Open PowerShell as administrator

Start menu, type `PowerShell`, right-click it, "Run as administrator".

## 2. Find the Lifeboat partition

```powershell
Get-Partition | Where-Object { $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' -and $_.IsHidden } |
  Format-Table DiskNumber, PartitionNumber, Size, IsHidden
```

It is a GPT "basic data" partition that is hidden and has no drive letter. Pick the one on
your backup drive (check `Get-Disk` for the drive's name and size). Note its `DiskNumber`
and `PartitionNumber`. Do not touch any other disk or partition.

## 3. Unhide it and give it a drive letter

```powershell
$d = 2; $p = 3     # put YOUR DiskNumber and PartitionNumber here
Set-Partition -DiskNumber $d -PartitionNumber $p -IsHidden $false
Add-PartitionAccessPath -DiskNumber $d -PartitionNumber $p -AssignDriveLetter
(Get-Partition -DiskNumber $d -PartitionNumber $p).DriveLetter
```

The last line prints the letter, for example `X`. If the partition is offline, bring the
disk online first with `Set-Disk -Number $d -IsOffline $false`.

## 4. List your backups

```powershell
$env:RESTIC_PASSWORD = Read-Host "Password or recovery key"
restic -r X:\repo snapshots
```

(Replace `X` with your letter.) Each line is one backup with an ID, the date, the PC name
and the folders. Backups made on any PC show up.

If you get "Access is denied", the partition is locked to the Lifeboat service. Open
PowerShell as SYSTEM or take ownership of `X:\repo` (`takeown /f X:\repo /r`), then retry.

## 5. Restore

```powershell
restic -r X:\repo restore <id> --target C:\Recovered
```

Use `latest` instead of an ID for the newest backup. To restore only one folder add
`--include "/C/Users/YourName/Documents"`. Always restore to a new folder, never over files
you still need. You can browse a backup first with `restic -r X:\repo ls <id>`.

## 6. Put the drive back the way it was

```powershell
Remove-PartitionAccessPath -DiskNumber $d -PartitionNumber $p -AccessPath "X:\"
Set-Partition -DiskNumber $d -PartitionNumber $p -IsHidden $true
Remove-Item Env:RESTIC_PASSWORD
```

The drive is hidden again and safe from ransomware. Do not delete, format or repartition
the drive: that destroys the backups.
