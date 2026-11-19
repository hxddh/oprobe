# oprobe installer for Windows.
#   irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 | iex
# Installs to %LOCALAPPDATA%\Programs\oprobe and adds it to the user PATH.
$ErrorActionPreference = 'Stop'
$repo = 'hxddh/oprobe'
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$target = "$arch-pc-windows-msvc"
$version = $env:OPROBE_VERSION
if (-not $version) {
  $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
$version = $version.TrimStart('v')
$name = "oprobe-$version-$target"
$base = "https://github.com/$repo/releases/download/v$version"
$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ([guid]::NewGuid()))
try {
  Write-Host "Downloading $name.zip"
  Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
  Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
  $expected = (Select-String -Path "$tmp\SHA256SUMS" -Pattern " $name.zip$").Line.Split(' ')[0]
  $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
  if ($expected -ne $actual) { throw "checksum mismatch for $name.zip" }
  Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp
  $dest = Join-Path $env:LOCALAPPDATA 'Programs\oprobe'
  New-Item -ItemType Directory -Force -Path $dest | Out-Null
  Copy-Item "$tmp\$name\*" $dest -Recurse -Force
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (($userPath -split ';') -notcontains $dest) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$dest", 'User')
    Write-Host "Added $dest to your PATH (restart the terminal)."
  }
  Write-Host "Installed oprobe $version to $dest\oprobe.exe"
} finally {
  Remove-Item -Recurse -Force $tmp
}
