$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = if ($env:SAMISHISA_REPO) { $env:SAMISHISA_REPO } else { 'heyngra/samishisa' }
$version = if ($env:SAMISHISA_VERSION) { $env:SAMISHISA_VERSION } else { 'latest' }
$dir = if ($env:SAMISHISA_INSTALL_DIR) { $env:SAMISHISA_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'samishisa\bin' }
$asset = 'samishisa-windows-x64.exe'
$base = if ($env:SAMISHISA_BASE_URL) { $env:SAMISHISA_BASE_URL.TrimEnd('/') } elseif ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download" } else { "https://github.com/$repo/releases/download/$version" }

[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("samishisa-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    Write-Host "downloading $asset ($version) from $base ..."
    Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile (Join-Path $tmp 'samishisa.exe')
    Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')

    $line = Get-Content (Join-Path $tmp 'SHA256SUMS') | Where-Object { $_ -match "\s\*?$([regex]::Escape($asset))$" } | Select-Object -First 1
    if (-not $line) { throw "$asset is not listed in SHA256SUMS" }
    $expected = ($line -split '\s+')[0].ToLower()
    $actual = (Get-FileHash (Join-Path $tmp 'samishisa.exe') -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "checksum mismatch for $asset (expected $expected, got $actual)" }

    New-Item -ItemType Directory -Force $dir | Out-Null
    Move-Item -Force (Join-Path $tmp 'samishisa.exe') (Join-Path $dir 'samishisa.exe')
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($userPath -split ';') -contains $dir)) {
    [Environment]::SetEnvironmentVariable('Path', ($(if ($userPath) { "$userPath;" } else { '' }) + $dir), 'User')
    Write-Host "added $dir to your user PATH (new terminals pick it up)"
}
if (-not (($env:Path -split ';') -contains $dir)) { $env:Path = "$env:Path;$dir" }

$v = & (Join-Path $dir 'samishisa.exe') version
Write-Host "installed $v to $dir\samishisa.exe"
Write-Host ""
Write-Host "Next:"
Write-Host "  samishisa init my-api; cd my-api; samishisa    # then open http://localhost:3000/docs"
Write-Host ""
Write-Host "Note: on Windows, extensions are sandboxed exactly as on Linux (WebAssembly), no Docker needed."
