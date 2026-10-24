# VM test runbook (M7)

Run this on a throwaway Windows 11 VM (Hyper-V or VirtualBox) that you can delete afterwards. Never run it on a real machine: it installs a LocalSystem service and creates and destroys disk partitions.

Tick each box as the expected result happens. If a step fails, stop and write down the step number and what you saw.

## 0. Prepare the VM

- [ ] Create a Windows 11 VM with 4 GB RAM, 60 GB disk, and take a checkpoint/snapshot named `clean`.
- [ ] Sign in as an administrator account.
- [ ] Install Rust (rustup, default MSVC toolchain) and Git. Open a new "Developer PowerShell" so `cargo` and `git` are on PATH.
- [ ] Add a second, empty virtual disk of at least 20 GB (Hyper-V: new VHDX on the SCSI controller; VirtualBox: new VDI). Initialize it later in step 3, not now.
- [ ] `git clone https://github.com/k1ri2o/lifeboat; cd lifeboat`
- [ ] `powershell -ExecutionPolicy Bypass -File scripts\fetch-restic.ps1` ends with restic in `vendor\restic\restic.exe`.

## 1. Tests that need no admin

- [ ] `cargo test --workspace` ends with `test result: ok` for every crate, no FAILED.
- [ ] The output has no line starting with `SKIPPED:` (that line means restic is missing).
- [ ] `cargo test -p lifeboat-detect --test simulate` passes (attack simulations: ransomware, slow attack, mass delete, normal day).
- [ ] `cargo test -p lifeboat-svc sim_tests` passes (real restic: the attack leaves the clean snapshot, the recovery key works).

Known open bugs (ignored tests, expected to fail if run with `-- --ignored`):

- [ ] `mass_delete_90_percent_freezes_through_the_service_scan` fails: the service scan cannot see deleted files.
- [ ] `full_disk_reports_a_clear_disk_full_error` fails: no clear "disk full" message yet.

## 2. Admin VHDX test

Open PowerShell **as Administrator** in the repo folder.

- [ ] `$env:LIFEBOAT_VHDX_TESTS=1; cargo test -p lifeboat-disk --test vhdx -- --ignored --nocapture`
- [ ] The test creates a temporary VHDX, partitions it, hides it and removes it again; it ends with `test result: ok`.
- [ ] Afterwards Disk Management shows no leftover virtual disk and no new drive letter in Explorer.

## 3. Install the service

- [ ] `cargo build --release -p lifeboat-svc` finishes.
- [ ] `.\target\release\lifeboat-svc.exe install` prints `installed service`.
- [ ] `Get-Service Lifeboat*` shows the service. Start it: `Start-Service <name>`; status becomes `Running`.
- [ ] Reboot the VM. After sign-in the service is `Running` again (automatic start).
- [ ] Event Viewer, Windows Logs, Application has no Lifeboat errors.

## 4. Real pipe check (admin)

- [ ] Run the app (`npm install; npm run tauri dev` in `app`, or the built exe). The status line says the service is connected.
- [ ] Stop the service (`Stop-Service`). The app says the service is not reachable within a few seconds, with no crash.
- [ ] Start it again. The app reconnects on its own.
- [ ] From a **non-admin** PowerShell, the app still connects (the pipe is open to normal users, the disk actions are not).

## 5. Manual UI pass

- [ ] The main window opens; text is readable, nothing is cut off at 100% and 150% scaling.
- [ ] Choose folders to protect: pick `C:\Users\<you>\Documents`. They appear in the list and survive closing and reopening the app.
- [ ] Prepare the backup disk on the second virtual disk. A clear confirmation is shown first. The disk does not get a drive letter in Explorer afterwards.
- [ ] Set a password, then write down the recovery key. The key is shown once.
- [ ] Press "Back up now". A snapshot appears in the list with the right time and size.
- [ ] Closing the window leaves the tray icon. Right click shows the menu; "Open" restores the window.
- [ ] A notification appears when a backup finishes, and a different, urgent one when an attack is detected (step 6).
- [ ] Restore one file to a new folder. The bytes match the original (compare with `Get-FileHash`).
- [ ] "Forgot password": the recovery key opens the repo; a wrong key shows a plain error and nothing else.

## 6. Attacks against the live service

Use a folder you protected in step 5, with at least 50 files of mixed types (docx, jpg, pdf, txt). Run one backup first so a clean snapshot exists.

### 6a. Fast ransomware

- [ ] In PowerShell, encrypt and rename everything:
  ```powershell
  Get-ChildItem C:\Test\Docs -File | ForEach-Object {
    $b = New-Object byte[] $_.Length; (New-Object Random).NextBytes($b)
    [IO.File]::WriteAllBytes($_.FullName, $b); Rename-Item $_.FullName ($_.Name + ".locked")
  }
  "Your files are encrypted" | Set-Content C:\Test\Docs\README_DECRYPT.txt
  ```
- [ ] Within one backup interval (or after "Back up now") the app shows the red "attack detected, backups paused" state and the urgent notification.
- [ ] The snapshot list has no new snapshot from after the attack, and the last clean one is marked `pre-alert`.
- [ ] Pressing "Back up now" while frozen does nothing and says why.
- [ ] Restore the `pre-alert` snapshot to a new folder: the original files are back byte for byte.
- [ ] Unfreeze from the app (with a confirmation). Backups resume.

### 6b. Slow ransomware

- [ ] Reset the folder (restore, back up). Then every few minutes, encrypt and rename only 2 or 3 files, running "Back up now" after each batch.
- [ ] The app freezes within a few batches (by the renamed `.locked` files or a touched canary file).

### 6c. Mass delete

- [ ] Reset the folder. Delete 90 percent of the files, then run "Back up now".
- [ ] Expected target: the app freezes. Known bug today: it does not (see step 1). Record what you see.

### 6d. Normal day (must not freeze)

- [ ] Reset the folder. Edit a few documents, add 10 photos, rename one file, run "Back up now".
- [ ] A new snapshot appears, no freeze, no alert.

### 6e. Full disk

- [ ] Fill the backup disk (copy large files in until nearly full), then run "Back up now".
- [ ] The app shows an error about the disk being full and asks to free space. Known gap today: the message is raw restic text.
- [ ] Older snapshots are all still listed and restorable (nothing was pruned because of the failure).

## 7. Clean up

- [ ] `.\target\release\lifeboat-svc.exe uninstall` prints `uninstalled service`.
- [ ] Revert the VM to the `clean` checkpoint, or delete it.
