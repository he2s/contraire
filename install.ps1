# Install the contraire binary on Windows (dashboard server and client in one file).
#
#   irm https://raw.githubusercontent.com/he2s/contraire/main/install.ps1 | iex
#
# Downloads the latest release, verifies its SHA-256 against SHA256SUMS, installs to
# %LOCALAPPDATA%\contraire and adds that folder to your user PATH.
$ErrorActionPreference = 'Stop'
$repo = if ($env:CONTRAIRE_REPO) { $env:CONTRAIRE_REPO } else { 'he2s/contraire' }
$version = if ($env:CONTRAIRE_VERSION) { $env:CONTRAIRE_VERSION } else { 'latest' }
$base = if ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download" } else { "https://github.com/$repo/releases/download/$version" }
$asset = 'contraire-windows-amd64.exe'
$dir = Join-Path $env:LOCALAPPDATA 'contraire'
$exe = Join-Path $dir 'contraire.exe'
New-Item -ItemType Directory -Force -Path $dir | Out-Null

Write-Host "Downloading $asset ($version) ..."
$tmp = "$exe.download"
Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $tmp
$sums = [Text.Encoding]::UTF8.GetString((Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS").Content)
$line = $sums -split "`n" | Where-Object { $_ -match ([regex]::Escape($asset) + '\s*$') } | Select-Object -First 1
if (-not $line) { Remove-Item $tmp; throw "$asset is not listed in SHA256SUMS" }
$expected = ($line -split '\s+')[0].ToLower()
$actual = (Get-FileHash -Algorithm SHA256 -Path $tmp).Hash.ToLower()
if ($actual -ne $expected) { Remove-Item $tmp; throw "checksum mismatch for $asset (expected $expected, got $actual)" }
Write-Host 'Checksum verified.'
Move-Item -Force -Path $tmp -Destination $exe

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $dir) {
  [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$dir"), 'User')
  Write-Host "Added $dir to your PATH. Open a new terminal to use 'contraire'."
}
Write-Host ("Installed " + (& $exe --version) + " to $exe")
Write-Host ''
Write-Host 'Next:'
Write-Host '  contraire ps --help      # control a contraire dashboard from here'
