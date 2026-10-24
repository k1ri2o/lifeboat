# Lifeboat — plan

Working name. A small, open-source Windows app: pick folders, and it keeps encrypted,
versioned backups of them on a hidden partition (internal disk or external drive) that
ransomware and viruses can't easily reach or ruin.

## 1. What can go wrong (threat model)

| # | Scenario | What a naive backup does | What Lifeboat does |
|---|----------|--------------------------|--------------------|
| 1 | Ransomware encrypts your files | Copies the encrypted junk over the good backup. Everything is lost. | Backups are **versioned snapshots that are never overwritten**. Old good versions stay. A detector notices the attack and **freezes** backups. |
| 2 | A virus-infected file gets backed up | — | The backup stores bytes encrypted; nothing in it ever runs, so a virus can't spread from it or "escape". Danger is only on restore, so restores go to a separate folder and get scanned by Windows Defender first. |
| 3 | Malware tries to delete or wipe the backup | Deletes it (it's just a folder) | Backup lives on a partition with **no drive letter**, owned by a Windows **service account**; normal programs (and most ransomware, which runs as the user) get "access denied". Deleting snapshots needs the password **and** waits 7 days. |
| 4 | Malware with admin rights | Deletes everything | Honest limit: admin malware can wipe any disk that is plugged in. Defence: an **external drive that is taken offline** after each backup (and a reminder to unplug it), plus an optional second copy elsewhere later. The app says this plainly. |
| 5 | Wiper deletes thousands of files | Mirrors the deletion | Snapshots keep deleted files; mass deletion triggers the same freeze as #1. |
| 6 | Slow corruption / dying disk | Backs up corrupt files | Old versions kept; weekly integrity check of the backup itself. Warns when backup and source are on the **same physical disk** (one disk failure kills both). |
| 7 | Backup disk fills up | Fails or deletes randomly | Retention (keep hourly 48, daily 30, weekly 12, monthly 24). **Pruning never runs while an alert is open**, and never removes the last snapshot taken before an alert. |
| 8 | Dad forgets the password | Backup is useless | A printable **recovery key** made at setup (second key on the repository). |
| 9 | Locked files (Outlook, open documents) | Skipped or half-copied | Service uses Volume Shadow Copy so it copies a consistent version. |
| 10 | Someone steals the disk | Reads everything | Everything is encrypted (AES-256, key from password with a strong key derivation); file names too. |
| 11 | Fake update of the app itself | — | Signed releases, checksums published, no silent auto-update by default. |

## 2. Ransomware detection (the "freeze")

Checked on every backup run, before a snapshot is kept as "good":
- Share of changed files far above normal (e.g. >30% of a folder, or >200 files in minutes).
- **Entropy** of changed files jumps to ~8 bits/byte (encrypted data looks random; documents don't).
- **File type no longer matches** its content (a `.docx` that no longer starts with `PK`, a `.jpg` without the JPEG header).
- Many renames to new extensions (`.locked`, `.crypt`, …), ransom-note files appearing.
- **Canary files**: hidden bait files in each watched folder; if one changes, it's an attack.

On a hit: stop taking snapshots, lock pruning, show a big red alert and a Windows notification,
and offer **"Restore everything to how it was before <time>"**, which picks the last snapshot
before the alert.

## 3. The hidden encrypted partition

- **External drive (recommended):** the app creates a GPT partition with a non-standard type,
  "hidden" + "no drive letter" attributes. Windows doesn't show it in Explorer. The service
  reaches it by its volume ID only, then **sets the disk offline** when the backup is done.
- **Internal disk:** the app shrinks a partition (Windows' own resize, only after checking free
  space and showing exactly what will happen) and creates the same hidden partition. Clear
  warning that this doesn't protect against a disk failure.
- The partition is NTFS with permissions only for the Lifeboat service. Encryption is done by the
  backup engine, so it works on Windows Home (no BitLocker needed).
- "Hidden" is a speed bump, not the real protection; the real protection is #1 (never-overwritten
  snapshots), #3 (permissions + delayed deletes) and #4 (offline disk).

## 4. Architecture (lightweight)

- **UI: Tauri 2** (Rust + the WebView2 already in Windows). Installer ~10 MB, idle RAM a fraction of
  Electron's. The UI only runs while open.
- **Service: Rust Windows service** (`lifeboat-svc`), runs as its own account, does the watching,
  scheduling, detection, partition work and backups. Talks to the UI over a local named pipe whose
  commands can't delete anything without password + delay.
- **Pipe gating:** PrepareDisk and Restore need the backup password (checked by the service against
  the repo; 5 wrong tries lock it for 15 minutes), and PrepareDisk also needs an elevated admin client
  (checked by impersonating the pipe client). Backup, status and listing stay open.
- **Backup engine: restic** (open source, BSD-2, audited design, encrypted, deduplicated snapshots),
  bundled as one exe. We don't write our own crypto.
- **Watching:** file-change events with a 10-minute quiet period, plus a scheduled hourly run.
- **Restore:** browse by date, restore a file/folder/everything to a new folder, Defender scan first.
- Windows first. macOS/Linux later if people want it.

## 5. Milestones

1. Skeleton: Tauri app + Rust service + installer, CI, MIT licence.
2. Engine: bundled restic, create repo, back up chosen folders, list and restore snapshots.
3. Hidden partition: external-drive and internal-disk setup, offline after backup, service-only permissions.
4. Ransomware detection + freeze + "restore to before the attack".
5. Retention, integrity checks, delayed deletes, recovery key printout.
6. Simple UI for a non-technical user (big buttons, plain words), tray icon, notifications.
7. Tests in a Windows VM: simulated ransomware, mass delete, full disk, forgotten password.
8. Public release on GitHub (needs your go-ahead).
