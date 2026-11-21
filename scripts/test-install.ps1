# End-to-end test of scripts/install.ps1 against a local fake release (CI: `install-script` job,
# Windows). Serves a fake release with python's http.server, installs from latest.json and with
# OPROBE_VERSION, and checks the checksum, the user PATH and this session's $env:Path.
#   pwsh scripts/test-install.ps1
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$installer = Join-Path $PSScriptRoot 'install.ps1'
$work = New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid()))
$arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -eq 'Arm64') { 'aarch64' } else { 'x86_64' }
$target = "$arch-pc-windows-msvc"
$server = $null
$savedUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')

function New-FakeRelease([string]$v) {
  $name = "oprobe-$v-$target"
  $stage = New-Item -ItemType Directory -Force -Path (Join-Path $work "stage\$name")
  Set-Content -Path (Join-Path $stage 'oprobe.exe') -Value "fake oprobe $v"
  $dir = New-Item -ItemType Directory -Force -Path (Join-Path $work "root\releases\download\v$v")
  Compress-Archive -Path $stage -DestinationPath (Join-Path $dir "$name.zip") -Force
  $hash = (Get-FileHash (Join-Path $dir "$name.zip") -Algorithm SHA256).Hash.ToLowerInvariant()
  Set-Content -Path (Join-Path $dir 'SHA256SUMS') -Value "$hash  $name.zip" -NoNewline
}

try {
  New-FakeRelease '9.1.0'
  New-FakeRelease '9.0.0'
  $latest = New-Item -ItemType Directory -Force -Path (Join-Path $work 'root\releases\latest\download')
  Set-Content -Path (Join-Path $latest 'latest.json') -Value '{"schema":"oprobe.release/1","version":"9.1.0","tag":"v9.1.0","assets":[]}'

  $l = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
  $l.Start(); $port = $l.LocalEndpoint.Port; $l.Stop()
  $py = if (Get-Command python -ErrorAction SilentlyContinue) { 'python' } else { 'python3' }
  $server = Start-Process -PassThru -WindowStyle Hidden -FilePath $py `
    -ArgumentList @('-m', 'http.server', "$port", '--bind', '127.0.0.1', '--directory', (Join-Path $work 'root'))
  $base = "http://127.0.0.1:$port/releases"
  for ($i = 0; $i -lt 100; $i++) {
    try { Invoke-WebRequest -UseBasicParsing "$base/latest/download/latest.json" | Out-Null; break } catch { Start-Sleep -Milliseconds 100 }
  }

  # latest.json, PATH updated for the user and this session.
  $dest = Join-Path $work 'install-a'
  $env:OPROBE_RELEASE_BASE = $base
  $env:OPROBE_API = "http://127.0.0.1:$port/api"
  $env:OPROBE_INSTALL_DIR = $dest
  Remove-Item Env:OPROBE_VERSION -ErrorAction SilentlyContinue
  Remove-Item Env:OPROBE_MODIFY_PATH -ErrorAction SilentlyContinue
  & $installer
  $got = Get-Content (Join-Path $dest 'oprobe.exe') -Raw
  if ($got.Trim() -ne 'fake oprobe 9.1.0') { throw "latest.json: installed '$got'" }
  if (-not (($env:Path -split ';') -contains $dest)) { throw "the session PATH lacks $dest" }
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not (($userPath -split ';') -contains $dest)) { throw "the user PATH lacks $dest" }
  Write-Host "ok: latest.json -> 9.1.0, PATH updated"

  # OPROBE_VERSION pin, OPROBE_MODIFY_PATH=0.
  $dest = Join-Path $work 'install-b'
  $env:OPROBE_INSTALL_DIR = $dest
  $env:OPROBE_VERSION = 'v9.0.0'
  $env:OPROBE_MODIFY_PATH = '0'
  & $installer
  $got = Get-Content (Join-Path $dest 'oprobe.exe') -Raw
  if ($got.Trim() -ne 'fake oprobe 9.0.0') { throw "pinned: installed '$got'" }
  if (([Environment]::GetEnvironmentVariable('Path', 'User') -split ';') -contains $dest) { throw 'OPROBE_MODIFY_PATH=0 changed the user PATH' }
  Write-Host "ok: OPROBE_VERSION -> 9.0.0, PATH untouched"

  # A tampered archive is refused.
  Set-Content -Path (Join-Path $work 'root\releases\download\v9.0.0\SHA256SUMS') -Value ('0' * 64 + "  oprobe-9.0.0-$target.zip")
  $env:OPROBE_INSTALL_DIR = Join-Path $work 'install-c'
  $refused = $false
  try { & $installer } catch { $refused = $true }
  if (-not $refused) { throw 'a checksum mismatch was installed' }
  Write-Host 'ok: checksum mismatch refused'
  Write-Host 'install.ps1: all cases passed'
} finally {
  [Environment]::SetEnvironmentVariable('Path', $savedUserPath, 'User')
  if ($server) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
  Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
