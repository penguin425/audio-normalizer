param(
    [Parameter(Mandatory = $true)]
    [string]$Target
)

$ErrorActionPreference = "Stop"
$targetDirectory = Join-Path "target" (Join-Path $Target "debug")
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("forge-c-api-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null

try {
    cargo build --locked --target $Target --lib
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed"
    }

    $dynamicLibrary = Join-Path $targetDirectory "forge_normalizer.dll"
    if (-not (Test-Path $dynamicLibrary)) {
        throw "missing $dynamicLibrary"
    }
    $importLibrary = Get-ChildItem -Path $targetDirectory -Filter "forge_normalizer*.lib" |
        Select-Object -First 1
    if ($null -eq $importLibrary) {
        throw "missing Forge import library"
    }

    $output = Join-Path $temporary "c-api-consumer.exe"
    cl.exe /nologo /std:c11 /W4 /WX /Iinclude tests/fixtures/c_api_consumer.c `
        /Fe:$output /link $importLibrary.FullName
    if ($LASTEXITCODE -ne 0) {
        throw "C consumer link failed"
    }
    Copy-Item $dynamicLibrary $temporary
    & $output
    if ($LASTEXITCODE -ne 0) {
        throw "C consumer returned $LASTEXITCODE"
    }
}
finally {
    if (Test-Path $temporary) {
        Remove-Item -Recurse -Force $temporary
    }
}
