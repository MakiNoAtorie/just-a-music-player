param([ValidatePattern('^[A-Za-z0-9][A-Za-z0-9-]*$')][string]$PackageName = 'Maki')
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
Push-Location $projectRoot
try {
    & "$PSScriptRoot\cargo.ps1" build --release --locked --offline
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    $packageRoot = Join-Path $projectRoot "dist\$PackageName"
    # Start clean so license files of removed dependency versions do not ship in the zip.
    if (Test-Path -LiteralPath $packageRoot) { Remove-Item -LiteralPath $packageRoot -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $packageRoot, "$packageRoot\licenses" | Out-Null
    Copy-Item -LiteralPath 'target\release\maki-player.exe' -Destination $packageRoot
    foreach ($name in @('README.md', 'VALIDATION.md', 'BUILD_SPEC.md', 'LICENSE')) { Copy-Item -LiteralPath $name -Destination $packageRoot }
    Copy-Item -LiteralPath 'licenses\Slint-Royalty-free-2.0.md' -Destination "$packageRoot\licenses"
    $metadata = (& "$PSScriptRoot\cargo.ps1" metadata --format-version 1 --locked --offline | Out-String | ConvertFrom-Json)
    if ($LASTEXITCODE -ne 0) { throw 'Could not read dependency metadata.' }
    $notices = [System.Collections.Generic.List[string]]::new()
    $notices.Add('# Dependency license inventory')
    $notices.Add('')
    $notices.Add('Includes every resolved dependency in Cargo.lock, including build tools and other platforms. Maki uses the Slint royalty-free option for Slint toolkit crates. Other dependencies retain their individual licenses.')
    $notices.Add('')
    foreach ($dependency in ($metadata.packages | Sort-Object name, version)) {
        if ($dependency.name -eq 'maki-player') { continue }
        $notices.Add("- $($dependency.name) $($dependency.version): $($dependency.license)")
        $sourceRoot = Split-Path $dependency.manifest_path -Parent
        foreach ($license in (Get-ChildItem -LiteralPath $sourceRoot -File | Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE)' })) {
            Copy-Item -LiteralPath $license.FullName -Destination (Join-Path "$packageRoot\licenses" "$($dependency.name)-$($dependency.version)-$($license.Name)")
        }
        $licenseDirectory = Join-Path $sourceRoot 'LICENSES'
        if (Test-Path -LiteralPath $licenseDirectory) {
            foreach ($license in (Get-ChildItem -LiteralPath $licenseDirectory -File)) {
                Copy-Item -LiteralPath $license.FullName -Destination (Join-Path "$packageRoot\licenses" "$($dependency.name)-$($dependency.version)-$($license.Name)")
            }
        }
        if ($dependency.license_file) {
            $licensePath = Join-Path $sourceRoot $dependency.license_file
            if (Test-Path -LiteralPath $licensePath) { Copy-Item -LiteralPath $licensePath -Destination (Join-Path "$packageRoot\licenses" "$($dependency.name)-$($dependency.version)-license.txt") }
        }
    }
    $notices | Set-Content -LiteralPath "$packageRoot\THIRD_PARTY_NOTICES.md"
    $zipEpoch = [datetime]::new(1980, 1, 1)
    Get-ChildItem -LiteralPath "$packageRoot\licenses" -File | Where-Object { $_.LastWriteTime -lt $zipEpoch } | ForEach-Object { $_.LastWriteTime = $zipEpoch }
    $zipPath = "dist\$PackageName-windows-x64.zip"
    Compress-Archive -Path "$packageRoot\*" -DestinationPath $zipPath -Force
    Get-Item -LiteralPath $zipPath | Select-Object FullName, Length
} finally { Pop-Location }
