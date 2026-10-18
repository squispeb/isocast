# Build an unsigned qualification installer from an already-built, locked tree.
# Does not build Rust, sign, publish, install Isocast or change host settings.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinDir,
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$ToolsDir,
    [Parameter(Mandatory)][string]$CrtDir,
    [string]$Version
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = (Resolve-Path "$PSScriptRoot\..\..").Path
if (-not $Version) {
    $manifest = Get-Content "$repo\Cargo.toml" -Raw
    $Version = [regex]::Match($manifest, '(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"$').Groups[1].Value
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Version must be X.Y.Z' }
$BinDir = (Resolve-Path $BinDir).Path
$CrtDir = (Resolve-Path $CrtDir).Path
foreach ($name in @('isocast.exe', 'isocastd.exe', 'isocast-ui.exe')) {
    if (-not (Test-Path -LiteralPath "$BinDir\$name" -PathType Leaf)) { throw "Missing executable: $BinDir\$name" }
}
if (-not (Test-Path "$CrtDir\vcruntime140.dll")) { throw 'CrtDir must be the x64 Microsoft Visual C++ redistributable CRT directory' }
if (Test-Path $OutputDir) { throw 'OutputDir must not exist; keep each package build separate' }
$OutputDir = (New-Item -ItemType Directory $OutputDir).FullName
$ToolsDir = (New-Item -ItemType Directory -Force $ToolsDir).FullName
$payload = (New-Item -ItemType Directory "$OutputDir\payload").FullName

# Inno Setup's licence permits this qualification without a purchase. Pin the
# compiler and its digest, rather than trusting the runner's preinstalled copy.
$innoVersion = '7.1.0'
$innoHash = '0362a383ed217d4c4239b5933866dd96d3eb2102737da92f80f6057a4b40df2f'
$installer = "$ToolsDir\innosetup-$innoVersion-x64.exe"
if (-not (Test-Path $installer)) {
    Invoke-WebRequest "https://github.com/jrsoftware/issrc/releases/download/is-7_1_0/innosetup-$innoVersion-x64.exe" -OutFile $installer -UseBasicParsing
}
if ((Get-FileHash $installer -Algorithm SHA256).Hash -ne $innoHash) { throw 'Inno Setup download SHA-256 mismatch' }
$compilerDir = "$ToolsDir\inno-$innoVersion"
if (-not (Test-Path "$compilerDir\ISCC.exe")) {
    $process = Start-Process $installer -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CURRENTUSER', ('/DIR="{0}"' -f $compilerDir)) -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Inno Setup compiler installation failed: $($process.ExitCode)" }
}
foreach ($name in @('isocast.exe', 'isocastd.exe', 'isocast-ui.exe')) { Copy-Item "$BinDir\$name" $payload }
# App-local CRT avoids the machine-wide redistributable's admin prompt.
Copy-Item "$CrtDir\*.dll" $payload
foreach ($name in @('LICENSE-MIT', 'LICENSE-APACHE', 'THIRD_PARTY_NOTICES')) {
    Copy-Item "$repo\$name" $payload
}
$files = @(Get-ChildItem $payload -File | Sort-Object Name | ForEach-Object {
    [ordered]@{ name=$_.Name; bytes=$_.Length; sha256=(Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() }
})
$metadata = [ordered]@{
    version=$Version
    architecture='x64'
    signed=$false
    compiler="Inno Setup $innoVersion"
    compiler_download_sha256=$innoHash
    files=$files
}
$metadata | ConvertTo-Json -Depth 4 | Set-Content "$payload\package.json" -Encoding UTF8
& "$compilerDir\ISCC.exe" "/DAppVersion=$Version" "/DPayloadDir=$payload" "/DOutputDir=$OutputDir" "$PSScriptRoot\installer.iss"
if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed' }
$setup = "$OutputDir\Isocast-$Version-windows-x64-setup.exe"
$hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $([IO.Path]::GetFileName($setup))" | Set-Content "$setup.sha256" -Encoding ascii
Write-Output "Unsigned qualification installer: $setup"
Write-Output "SHA-256: $hash"
