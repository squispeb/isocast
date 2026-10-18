# Shared by packaging and appcast generation. Only downloads a pinned SDK.
function Get-WinSparkle([string]$ToolsDir) {
    $version = '0.9.4'
    $hash = '6037df37fc263bd1650a1c4949681a9d40ffe991d01f35892a406cb5d103c976'
    New-Item -ItemType Directory -Force $ToolsDir | Out-Null
    $archive = Join-Path $ToolsDir "WinSparkle-$version.zip"
    if (-not (Test-Path $archive)) {
        Invoke-WebRequest "https://github.com/vslavik/winsparkle/releases/download/v$version/WinSparkle-$version.zip" -OutFile $archive -UseBasicParsing
    }
    if ((Get-FileHash $archive -Algorithm SHA256).Hash -ne $hash) { throw 'WinSparkle SDK SHA-256 mismatch' }
    $folder = Join-Path $ToolsDir "WinSparkle-$version"
    if (-not (Test-Path "$folder\x64\Release\WinSparkle.dll")) {
        Expand-Archive -LiteralPath $archive -DestinationPath $ToolsDir -Force
    }
    return (Resolve-Path $folder).Path
}
