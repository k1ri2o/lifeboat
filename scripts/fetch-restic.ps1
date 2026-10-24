# Downloads a pinned restic release, verifies its SHA-256 and extracts it to vendor/restic/restic.exe.
$ErrorActionPreference = 'Stop'
$Version = '0.19.1'
# Official hash from https://github.com/restic/restic/releases/download/v0.19.1/SHA256SUMS
$Sha256 = 'da948ad707ed690426473aaba2046cd61f8f90f6f0e7dab6be0d5796531de67d'
$Root = Split-Path -Parent $PSScriptRoot
$Dir = Join-Path $Root 'vendor/restic'
$Exe = Join-Path $Dir 'restic.exe'
if (Test-Path $Exe) { Write-Host "restic already present: $Exe"; exit 0 }
New-Item -ItemType Directory -Force $Dir | Out-Null
$Zip = Join-Path $Dir 'restic.zip'
$Url = "https://github.com/restic/restic/releases/download/v$Version/restic_${Version}_windows_amd64.zip"
Invoke-WebRequest -Uri $Url -OutFile $Zip
$Actual = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
if ($Actual -ne $Sha256) {
    Remove-Item $Zip -Force
    throw "SHA-256 mismatch for restic zip: expected $Sha256 got $Actual"
}
Expand-Archive -Path $Zip -DestinationPath $Dir -Force
Remove-Item $Zip -Force
$Extracted = Get-ChildItem $Dir -Filter 'restic_*.exe' | Select-Object -First 1
if ($Extracted) { Move-Item $Extracted.FullName $Exe -Force }
if (-not (Test-Path $Exe)) { throw 'restic.exe not found after extraction' }
Write-Host "restic $Version installed to $Exe"
