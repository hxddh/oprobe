# oprobe installer for Windows.
#   irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 | iex
# Installs to %LOCALAPPDATA%\Programs\oprobe and adds it to the user PATH and to this session.
#
# Options (environment):
#   OPROBE_VERSION=4.0.0       install this version instead of the latest
#   OPROBE_INSTALL_DIR=DIR     where to install (default %LOCALAPPDATA%\Programs\oprobe)
#   OPROBE_MODIFY_PATH=0       do not touch the user PATH (default: add the directory)
#   OPROBE_RELEASE_BASE=URL    release download base (mirrors, tests)
#   OPROBE_API=URL             GitHub API base (default https://api.github.com)
#   OPROBE_VERIFY=auto|require|checksum
#                              signature policy after the SHA-256 check (default auto; see install.sh)
#   OPROBE_MINISIGN_PUBKEY=KEY minisign public key (overrides the embedded one)
#
# Verification (design v5 §5), after the SHA-256 check: the GitHub build provenance
# (`gh attestation verify`, when gh is installed and logged in), otherwise a minisign signature
# over SHA256SUMS (when minisign.exe is installed and the release is signed), otherwise a warning
# (an error with OPROBE_VERIFY=require). A verification that fails is always an error.
#
# The latest version is resolved without the GitHub API when possible, falling back on any failure:
#   1. <base>/latest/download/latest.json   2. the <base>/latest redirect   3. the GitHub API
$ErrorActionPreference = 'Stop'
# The progress bar makes Invoke-WebRequest many times slower on Windows PowerShell 5.1.
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repo = 'hxddh/oprobe'
$base = if ($env:OPROBE_RELEASE_BASE) { $env:OPROBE_RELEASE_BASE.TrimEnd('/') } else { "https://github.com/$repo/releases" }
$api = if ($env:OPROBE_API) { $env:OPROBE_API.TrimEnd('/') } else { 'https://api.github.com' }
$verify = if ($env:OPROBE_VERIFY) { $env:OPROBE_VERIFY } else { 'auto' }
if (@('auto', 'require', 'checksum') -notcontains $verify) { throw 'oprobe-install: OPROBE_VERIFY must be auto, require or checksum' }
# The release's minisign public key; the release workflow fills it in (docs/releasing.md#signing).
$embeddedPubkey = ''
$pubkey = if ($env:OPROBE_MINISIGN_PUBKEY) { $env:OPROBE_MINISIGN_PUBKEY } else { $embeddedPubkey }

# The OS architecture, not this process's: an x64 PowerShell under emulation on ARM64 reports
# PROCESSOR_ARCHITECTURE=AMD64, and a 32-bit one on x64 reports x86.
function Get-OsArch {
  try {
    $a = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    if ($a) { return $a.ToUpperInvariant() }
  } catch { }
  if ($env:PROCESSOR_ARCHITEW6432) { return $env:PROCESSOR_ARCHITEW6432.ToUpperInvariant() }
  return "$env:PROCESSOR_ARCHITECTURE".ToUpperInvariant()
}
$arch = switch (Get-OsArch) {
  'ARM64' { 'aarch64' }
  'X64' { 'x86_64' }
  'AMD64' { 'x86_64' }
  default { throw "oprobe-install: unsupported architecture $(Get-OsArch) (oprobe ships x64 and ARM64 builds)" }
}
$target = "$arch-pc-windows-msvc"

function Get-Version([string]$s) {
  if ($s -match '/tag/v?([0-9][^/?#]*)') { return $Matches[1] }
  return $null
}

function Resolve-Latest {
  # 1. latest.json (a release asset; no API, no rate limit).
  try {
    $doc = Invoke-RestMethod -UseBasicParsing "$base/latest/download/latest.json"
    if ($doc.version) { return $doc.version }
  } catch { }
  # 2. The releases/latest redirect: its Location (or the final URL) ends in /tag/vX.Y.Z.
  try {
    $req = [System.Net.HttpWebRequest]::Create("$base/latest")
    $req.Method = 'HEAD'
    $req.AllowAutoRedirect = $false
    $resp = $req.GetResponse()
    try {
      $v = Get-Version $resp.Headers['Location']
      if ($v) { return $v }
    } finally { $resp.Close() }
  } catch { }
  try {
    $r = Invoke-WebRequest -UseBasicParsing -Method Head "$base/latest"
    $final = $null
    if ($r.BaseResponse.ResponseUri) { $final = $r.BaseResponse.ResponseUri.AbsoluteUri }
    elseif ($r.BaseResponse.RequestMessage) { $final = $r.BaseResponse.RequestMessage.RequestUri.AbsoluteUri }
    $v = Get-Version "$final"
    if ($v) { return $v }
  } catch { }
  # 3. The GitHub API.
  try {
    $rel = Invoke-RestMethod -UseBasicParsing "$api/repos/$repo/releases/latest"
    if ($rel.tag_name) { return $rel.tag_name }
  } catch { }
  throw "oprobe-install: cannot determine the latest version (set `$env:OPROBE_VERSION, or download from https://github.com/$repo/releases)"
}

$version = $env:OPROBE_VERSION
if (-not $version) { $version = Resolve-Latest }
$version = $version.TrimStart('v')
$name = "oprobe-$version-$target"
$url = "$base/download/v$version"
$tmp = New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid()))
try {
  Write-Host "Downloading $name.zip"
  Invoke-WebRequest -UseBasicParsing "$url/$name.zip" -OutFile "$tmp\$name.zip"
  Invoke-WebRequest -UseBasicParsing "$url/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
  $line = Select-String -Path "$tmp\SHA256SUMS" -Pattern " \*?$([regex]::Escape("$name.zip"))$" | Select-Object -First 1
  if (-not $line) { throw "oprobe-install: SHA256SUMS does not list $name.zip" }
  $expected = $line.Line.Split(' ')[0].ToLowerInvariant()
  $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($expected -ne $actual) { throw "oprobe-install: checksum mismatch for $name.zip" }
  if ($verify -ne 'checksum') {
    $verified = $null
    $gh = Get-Command gh -ErrorAction SilentlyContinue
    $minisign = Get-Command minisign -ErrorAction SilentlyContinue
    $ghReady = $false
    if ($gh) { & $gh.Source auth status *> $null; $ghReady = ($LASTEXITCODE -eq 0); $global:LASTEXITCODE = 0 }
    if ($ghReady) {
      & $gh.Source attestation verify "$tmp\$name.zip" --repo $repo --signer-workflow "$repo/.github/workflows/release.yml"
      if ($LASTEXITCODE -ne 0) { throw "oprobe-install: the build provenance of $name.zip does not verify; nothing was installed" }
      $verified = 'GitHub build provenance (gh attestation verify)'
    } elseif ($pubkey -and $minisign) {
      $sig = $null
      try { Invoke-WebRequest -UseBasicParsing "$url/SHA256SUMS.minisig" -OutFile "$tmp\SHA256SUMS.minisig"; $sig = "$tmp\SHA256SUMS.minisig" } catch { }
      if ($sig) {
        & $minisign.Source -Vq -P $pubkey -m "$tmp\SHA256SUMS" -x $sig *> $null
        if ($LASTEXITCODE -ne 0) { throw 'oprobe-install: the minisign signature of SHA256SUMS does not verify; nothing was installed' }
        $verified = 'minisign signature over SHA256SUMS'
      }
    }
    if ($verified) { Write-Host "Verified: $verified" }
    elseif ($verify -eq 'require') { throw "oprobe-install: cannot verify the provenance or signature of $name.zip (install gh and run gh auth login), or set OPROBE_VERIFY=auto" }
    else { Write-Warning 'only the SHA-256 checksum was verified; install gh and run `gh auth login` to check the build provenance too' }
  }
  Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp -Force
  $dest = if ($env:OPROBE_INSTALL_DIR) { $env:OPROBE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\oprobe' }
  New-Item -ItemType Directory -Force -Path $dest | Out-Null
  Copy-Item "$tmp\$name\*" $dest -Recurse -Force
  Write-Host "Installed oprobe $version to $dest\oprobe.exe"

  $onPath = ($env:Path -split ';' | Where-Object { $_ -and ($_.TrimEnd('\') -ieq $dest.TrimEnd('\')) })
  if ($env:OPROBE_MODIFY_PATH -eq '0') {
    if (-not $onPath) { Write-Host "$dest is not on your PATH (OPROBE_MODIFY_PATH=0): add it to run oprobe." }
  } else {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @($userPath -split ';' | Where-Object { $_ })
    if (-not ($parts | Where-Object { $_.TrimEnd('\') -ieq $dest.TrimEnd('\') })) {
      [Environment]::SetEnvironmentVariable('Path', (($parts + $dest) -join ';'), 'User')
      Write-Host "Added $dest to your user PATH."
    }
    if (-not $onPath) {
      # This session too: `oprobe` works right away, no new terminal needed.
      $env:Path = "$($env:Path.TrimEnd(';'));$dest"
    }
  }
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
