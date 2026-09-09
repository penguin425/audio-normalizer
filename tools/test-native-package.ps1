[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Root,

    [Parameter(Mandatory = $true, Position = 1)]
    [string]$ExpectedVersion
)

$ErrorActionPreference = "Stop"

function Stop-NativePackageTest {
    param([Parameter(Mandatory = $true)][string]$Message)

    throw "native package test: $Message"
}

$releaseVersionPattern = '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
if ($ExpectedVersion -notmatch $releaseVersionPattern) {
    Stop-NativePackageTest "EXPECTED_VERSION must use numeric MAJOR.MINOR.PATCH without leading zeros: $ExpectedVersion"
}

try {
    $rootPath = (Resolve-Path -LiteralPath $Root -ErrorAction Stop).Path
}
catch {
    Stop-NativePackageTest "ROOT cannot be resolved: $Root"
}
if (-not (Test-Path -LiteralPath $rootPath -PathType Container)) {
    Stop-NativePackageTest "ROOT is not an existing directory: $Root"
}
if ($rootPath -eq [System.IO.Path]::GetPathRoot($rootPath)) {
    Stop-NativePackageTest "ROOT must not be a filesystem root"
}

$scriptDirectory = $PSScriptRoot
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $scriptDirectory "..")).Path
$fixtureDirectory = Join-Path $repoRoot "tests/fixtures/native_package"

foreach ($commandName in @("cmake", "cl.exe")) {
    if ($null -eq (Get-Command $commandName -CommandType Application -ErrorAction SilentlyContinue)) {
        Stop-NativePackageTest "missing required command: $commandName"
    }
}
$cmakeCommand = (Get-Command cmake -CommandType Application).Path

$temporary = Join-Path ([System.IO.Path]::GetTempPath()) (
    "forge-native-package-" + [guid]::NewGuid().ToString("N"))
$prefix = Join-Path $temporary "forge native package-日本語"
New-Item -ItemType Directory -Path "$prefix/include","$prefix/lib","$prefix/bin" -Force | Out-Null

try {
    Copy-Item -LiteralPath (Join-Path $rootPath "include/forge_normalizer.h") -Destination "$prefix/include/"
    Copy-Item -LiteralPath (Join-Path $rootPath "bin/forge_normalizer.dll") -Destination "$prefix/bin/"
    # Relocate only the canonical development payload. Native archives also
    # contain many CLI programs, but they do not participate in discovery.
    foreach ($child in @(Get-ChildItem -LiteralPath (Join-Path $rootPath "lib") -Force)) {
        Copy-Item -LiteralPath $child.FullName -Destination "$prefix/lib/" -Recurse
    }

    $cmakeConfigDirectory = Join-Path $prefix "lib/cmake/ForgeNormalizer"
    $metadataPaths = @(
        (Join-Path $cmakeConfigDirectory "ForgeNormalizerConfig.cmake"),
        (Join-Path $cmakeConfigDirectory "ForgeNormalizerConfigVersion.cmake")
    )
    foreach ($metadataPath in $metadataPaths) {
        if (-not (Test-Path -LiteralPath $metadataPath -PathType Leaf)) {
            Stop-NativePackageTest "missing package metadata: $metadataPath"
        }
    }

    $utf8 = [System.Text.UTF8Encoding]::new($false, $true)
    $rootVariants = @(
        $rootPath,
        $rootPath.Replace('\', '/'),
        $repoRoot,
        $repoRoot.Replace('\', '/')
    ) | Select-Object -Unique
    foreach ($metadataPath in $metadataPaths) {
        try {
            $metadataText = [System.IO.File]::ReadAllText($metadataPath, $utf8)
        }
        catch {
            Stop-NativePackageTest "metadata is not valid UTF-8: $metadataPath"
        }
        foreach ($forbiddenPath in $rootVariants) {
            if ($forbiddenPath -and $metadataText.Contains($forbiddenPath)) {
                Stop-NativePackageTest "metadata retains an original absolute path: $metadataPath"
            }
        }
        if ($metadataText -match '@[A-Za-z_][A-Za-z0-9_]*@') {
            Stop-NativePackageTest "metadata contains an unexpanded template token: $metadataPath"
        }
    }

    $cmakeBuild = Join-Path $temporary "cmake-build"
    $configureArguments = @(
        "-S", $fixtureDirectory,
        "-B", $cmakeBuild,
        "-DCMAKE_PREFIX_PATH=$prefix",
        "-DFORGE_EXPECTED_VERSION=$ExpectedVersion",
        "-DFORGE_TEST_PREFIX=$prefix",
        "-DCMAKE_SKIP_RPATH=ON",
        "-DCMAKE_BUILD_TYPE=Release"
    )
    & $cmakeCommand @configureArguments
    if ($LASTEXITCODE -ne 0) {
        Stop-NativePackageTest "CMake configure failed"
    }

    $buildArguments = @(
        "--build", $cmakeBuild,
        "--config", "Release",
        "--target", "native-package-cmake-consumer"
    )
    & $cmakeCommand @buildArguments
    if ($LASTEXITCODE -ne 0) {
        Stop-NativePackageTest "CMake build failed"
    }

    $cmakeConsumer = Get-ChildItem -LiteralPath $cmakeBuild -Filter "native-package-cmake-consumer.exe" -File -Recurse |
        Select-Object -First 1
    if ($null -eq $cmakeConsumer) {
        Stop-NativePackageTest "CMake consumer executable was not produced"
    }

    $runtimeDll = Join-Path $prefix "bin/forge_normalizer.dll"
    if (-not (Test-Path -LiteralPath $runtimeDll -PathType Leaf)) {
        Stop-NativePackageTest "missing moved runtime DLL: $runtimeDll"
    }
    Copy-Item -LiteralPath $runtimeDll -Destination $cmakeConsumer.DirectoryName -Force
    & $cmakeConsumer.FullName
    if ($LASTEXITCODE -ne 0) {
        Stop-NativePackageTest "CMake consumer failed with exit code $LASTEXITCODE"
    }

    Write-Host "native package: OK ($prefix)"
}
finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
}
