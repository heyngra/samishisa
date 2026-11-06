# Installs samishisa on Windows (PowerShell 5.1 or newer):
#
#   irm https://raw.githubusercontent.com/heyngra/samishisa/main/install.ps1 | iex
#
# Environment variables:
#   $env:SAMISHISA_VERSION = 'v0.1.0'                release to install (default: latest)
#   $env:SAMISHISA_INSTALL_DIR = 'C:\x'              install folder (default: %LOCALAPPDATA%\samishisa\bin)
#   $env:SAMISHISA_REPO = 'owner/repo'               install from a fork
#   $env:SAMISHISA_BASE_URL = 'https://...'          download from a mirror (a folder with the release files)
#   $env:SAMISHISA_URL = 'https://.../samishisa.exe' direct link to the binary (CDN, file server)
#   $env:SAMISHISA_SHA256 = '<hex>'                  checksum the download must match
#   $env:SAMISHISA_FILE = 'C:\x\samishisa.exe'       install a binary you already have, no network
#   $env:SAMISHISA_EXTENSION = 'https://.../mio.wasm'   also add this extension (any source `samishisa add` accepts)
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
    $exe = Join-Path $tmp 'samishisa.exe'
    if ($env:SAMISHISA_FILE) {
        Write-Host "using $($env:SAMISHISA_FILE) ..."
        Copy-Item -LiteralPath $env:SAMISHISA_FILE $exe
        if ($env:SAMISHISA_SHA256) { $verify = $env:SAMISHISA_SHA256 }
    } elseif ($env:SAMISHISA_URL) {
        Write-Host "downloading $($env:SAMISHISA_URL) ..."
        Invoke-WebRequest -UseBasicParsing $env:SAMISHISA_URL -OutFile $exe
        if ($env:SAMISHISA_SHA256) { $verify = $env:SAMISHISA_SHA256 }
        else { Write-Host "note: no SAMISHISA_SHA256 given, so the download is not verified" }
    } else {
        Write-Host "downloading $asset ($version) from $base ..."
        Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile $exe
        Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
        $line = Get-Content (Join-Path $tmp 'SHA256SUMS') | Where-Object { $_ -match "\s\*?$([regex]::Escape($asset))$" } | Select-Object -First 1
        if (-not $line) { throw "$asset is not listed in SHA256SUMS" }
        $verify = ($line -split '\s+')[0]
    }
    if ($verify) {
        $actual = (Get-FileHash $exe -Algorithm SHA256).Hash.ToLower()
        if ($verify.ToLower() -ne $actual) { throw "checksum mismatch (expected $($verify.ToLower()), got $actual)" }
    }

    New-Item -ItemType Directory -Force $dir | Out-Null
    Move-Item -Force $exe (Join-Path $dir 'samishisa.exe')
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
if ($env:SAMISHISA_EXTENSION) {
    $listed = & (Join-Path $dir 'samishisa.exe') list | Select-String -SimpleMatch " $($env:SAMISHISA_EXTENSION)"
    if ($listed) {
        Write-Host "$($env:SAMISHISA_EXTENSION) is already in your config"
    } else {
        Write-Host "adding $($env:SAMISHISA_EXTENSION) ..."
        & (Join-Path $dir 'samishisa.exe') add $env:SAMISHISA_EXTENSION
        if ($LASTEXITCODE -ne 0) { throw "samishisa add failed" }
    }
}
Write-Host "          MIGGIKKGEGGGGGGGGGGGKQ           
         KGGGGGGGGGGGGGGIGGGIMOQOY         
   YWY  WGEGGGIGGGGGGIGGGGGGGGKMQQW        
   WY Y  OEEGGGGGIIMQSSUUQQKIGGQQQQ        
   W   W  WIEGIKMQUUWUUUUUUUQOSSSQOW       
   W   W     OMOUUUUUWUUUOMMUQQOSQS        
   Y   Y   WSQSUUUUUWWWWUSOMOWSUUW   YYY   
   Y   YY  WWUSUUUWUSWWWWWWWWWUSUUYY     Y 
   Y   YYWOMSUSUSWYWSSSUWUWYUWWSUSSUY    W 
   Y    WOMMSUUSU    WYYY   WUWQQUYUUW YY  
  W     SOMMSUUQOUWSY   WUWSOSWSOQY UU     
  Y     YOMMOUUUW          YUWUWSUUUUY     
 WYY    YSUUUUSSUY  WYYYY YQUUU            
Y         YWWUQOQOW YYYY  WWUWW            
                SOS SWYWW WWW Y  S         
Y    Y           YUUQWWYW SUUU  UOY        
               YYUUYWW      WUSSW          
                 YW  Y Y      GIQ          
    Y            YU YUSSWUY  MGGI          
Y              YSIO UOQSMKW QGGGGO         
                YMS MIUWKIW SGGGGGS        
UY   Y   YYYWWYWMGY UQYYWMS  YOGGEEQ       
             WWSGG  SSYY UQ   WEGGEEU      "
Write-Host "Samisha has been installed! Next steps:"
Write-Host "  samishisa add <source>      # an extension: github:owner/repo, or the URL of a .wasm"
Write-Host "  samishisa config            # edit your config, manage extensions etc."
Write-Host "  samishisa add builtin:docs  # optional: interactive API docs at /docs"
Write-Host "  samishisa run               # run it on http://localhost:[port] (default 3000)"
