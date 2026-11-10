# Baut Packup fuer x64 und ARM64 und erzeugt daraus je eine MSI-Datei in dist\.
#
#   .\installer\build-msi.ps1              # EXE neu bauen + MSI erzeugen
#   .\installer\build-msi.ps1 -SkipCargo   # nur MSI aus den vorhandenen EXE-Dateien
#
# Voraussetzung: WiX Toolset 3.14 (Binaerpaket) in tools\wix314.
# Hinweis: Diese Datei bewusst ohne Umlaute halten (Windows PowerShell 5.1 liest sie sonst falsch).

param([switch]$SkipCargo)

# Herstellername, wie er unter "Apps" in Windows erscheint.
$Manufacturer = 'ZEBA Software UG'

$root = Split-Path -Parent $PSScriptRoot
$wix = Join-Path $root 'tools\wix314'
$wxs = Join-Path $PSScriptRoot 'Packup.wxs'
$icon = Join-Path $root 'assets\Packup.ico'
$dist = Join-Path $root 'dist'
$obj = Join-Path $root 'target\wix'

if (-not (Test-Path (Join-Path $wix 'candle.exe'))) {
    throw "WiX Toolset nicht gefunden: $wix"
}

# Die Versionsnummer kommt aus Cargo.toml, damit EXE und MSI immer zusammenpassen.
$match = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $match) { throw 'Versionsnummer in Cargo.toml nicht gefunden.' }
$version = $match.Matches[0].Groups[1].Value

$targets = @(
    @{ Arch = 'x64'; Triple = 'x86_64-pc-windows-msvc' },
    @{ Arch = 'arm64'; Triple = 'aarch64-pc-windows-msvc' }
)

New-Item -ItemType Directory -Force $dist, $obj | Out-Null

foreach ($target in $targets) {
    $arch = $target.Arch
    $exe = Join-Path $root "target\$($target.Triple)\release\Packup.exe"

    if (-not $SkipCargo) {
        Write-Host "== cargo build ($arch) =="
        cargo build --release --target $target.Triple --manifest-path (Join-Path $root 'Cargo.toml')
        if ($LASTEXITCODE -ne 0) { throw "cargo build fuer $arch fehlgeschlagen." }
    }
    if (-not (Test-Path $exe)) { throw "EXE fehlt: $exe" }

    $wixobj = Join-Path $obj "Packup-$arch.wixobj"
    $msi = Join-Path $dist "Packup-$version-$arch.msi"

    Write-Host "== MSI ($arch) =="
    & (Join-Path $wix 'candle.exe') -nologo -arch $arch "-dVersion=$version" "-dManufacturer=$Manufacturer" "-dExePath=$exe" "-dIconPath=$icon" -out $wixobj $wxs
    if ($LASTEXITCODE -ne 0) { throw "candle fuer $arch fehlgeschlagen." }

    & (Join-Path $wix 'light.exe') -nologo -spdb -out $msi $wixobj
    if ($LASTEXITCODE -ne 0) { throw "light fuer $arch fehlgeschlagen." }

    # Die einzelne EXE zusaetzlich nach dist kopieren. Laeuft sie gerade, ist sie gesperrt:
    # Dann nur hinweisen - die MSI-Datei ist davon nicht betroffen.
    $copy = Join-Path $dist "Packup-$arch.exe"
    try {
        Copy-Item $exe $copy -Force -ErrorAction Stop
    } catch {
        Write-Warning "$copy ist geoeffnet und wurde nicht ersetzt. Packup schliessen und das Skript erneut starten."
    }
    Write-Host "Fertig: $msi"
}
