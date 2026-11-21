# Builds dist\fast-search-windows-x86_64-setup.exe from a `cargo win`
# build, with Inno Setup 6 (installed with Chocolatey when missing, as on
# CI runners). The version is $env:FF_VERSION, else Cargo.toml's.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path "$PSScriptRoot\..\..").Path
$bin = Join-Path $root 'target\x86_64-pc-windows-msvc\release'

$version = $env:FF_VERSION
if (-not $version) {
    $version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
}

# The setup's icon, drawn by the same code as the window's.
$icon = Join-Path $bin 'fast-search.ico'
$p = Start-Process (Join-Path $bin 'ff-gui.exe') -ArgumentList "--write-icon `"$icon`"" -Wait -PassThru
if ($p.ExitCode -ne 0 -or -not (Test-Path $icon)) { throw "ff-gui --write-icon failed" }

$iscc = @("${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe", "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) {
    if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
        throw "Inno Setup 6 is not installed (winget install JRSoftware.InnoSetup)"
    }
    choco install innosetup -y --no-progress | Out-Null
    $iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
}

& $iscc /Q "/DAppVersion=$version" (Join-Path $PSScriptRoot 'fast-search.iss')
if ($LASTEXITCODE -ne 0) { throw "iscc failed ($LASTEXITCODE)" }
Write-Host "built dist\fast-search-windows-x86_64-setup.exe (version $version)"
