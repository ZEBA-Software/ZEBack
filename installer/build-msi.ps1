# Baut ZEBack fuer x64 und ARM64 und erzeugt daraus je eine MSI-Datei in dist\.
#
#   .\installer\build-msi.ps1              # EXE neu bauen + MSI erzeugen
#   .\installer\build-msi.ps1 -SkipCargo   # nur MSI aus den vorhandenen EXE-Dateien
#   .\installer\build-msi.ps1 -WixDir C:\WiX\bin   # WiX an anderer Stelle (z. B. bei GitHub Actions)
#
# Voraussetzung: WiX Toolset 3.14 (Binaerpaket) in tools\wix314 oder im Ordner aus -WixDir.
# Hinweis: Diese Datei bewusst ohne Umlaute halten (Windows PowerShell 5.1 liest sie sonst falsch).

param(
    [switch]$SkipCargo,
    # Ordner mit candle.exe und light.exe; ohne Angabe tools\wix314 im Projekt.
    [string]$WixDir
)

# Herstellername, wie er unter "Apps" in Windows erscheint.
$Manufacturer = 'ZEBA Software UG'

$root = Split-Path -Parent $PSScriptRoot
$wix = if ($WixDir) { $WixDir } else { Join-Path $root 'tools\wix314' }
$wxs = Join-Path $PSScriptRoot 'ZEBack.wxs'
$icon = Join-Path $root 'assets\ZEBack.ico'
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

# Bilder des Assistenten: build.rs erzeugt sie bei jedem cargo-Lauf aus dem Logo.
$banner = Join-Path $root 'assets\installer-banner.bmp'
$dialog = Join-Path $root 'assets\installer-dialog.bmp'

# Lizenztext fuer den Assistenten: die Datei LICENSE als einfaches RTF.
$license = Join-Path $obj 'License.rtf'
$text = [IO.File]::ReadAllText((Join-Path $root 'LICENSE'))
$text = $text.Replace('\', '\\').Replace('{', '\{').Replace('}', '\}')
$text = ($text -split "\r?\n") -join "\par`r`n"
[IO.File]::WriteAllText($license, '{\rtf1\ansi\deff0{\fonttbl{\f0\fnil Segoe UI;}}\fs18 ' + $text + '}', [Text.Encoding]::ASCII)

foreach ($target in $targets) {
    $arch = $target.Arch
    $exe = Join-Path $root "target\$($target.Triple)\release\ZEBack.exe"

    if (-not $SkipCargo) {
        Write-Host "== cargo build ($arch) =="
        cargo build --release --target $target.Triple --manifest-path (Join-Path $root 'Cargo.toml')
        if ($LASTEXITCODE -ne 0) { throw "cargo build fuer $arch fehlgeschlagen." }
    }
    if (-not (Test-Path $exe)) { throw "EXE fehlt: $exe" }
    foreach ($image in $banner, $dialog) {
        if (-not (Test-Path $image)) { throw "Bild fuer den Installer fehlt: $image (einmal 'cargo build' ausfuehren)." }
    }

    $wixobj = Join-Path $obj "ZEBack-$arch.wixobj"
    $msi = Join-Path $dist "ZEBack-$version-$arch.msi"

    Write-Host "== MSI ($arch) =="
    # WixUIExtension: der Assistent; WixUtilExtension: Programm nach der Installation starten.
    & (Join-Path $wix 'candle.exe') -nologo -arch $arch -ext WixUIExtension -ext WixUtilExtension "-dVersion=$version" "-dManufacturer=$Manufacturer" "-dExePath=$exe" "-dIconPath=$icon" "-dBannerPath=$banner" "-dDialogPath=$dialog" "-dLicensePath=$license" -out $wixobj $wxs
    if ($LASTEXITCODE -ne 0) { throw "candle fuer $arch fehlgeschlagen." }

    # Sprache des Assistenten: Englisch; muss zu Language="1033" in ZEBack.wxs passen.
    & (Join-Path $wix 'light.exe') -nologo -spdb -ext WixUIExtension -ext WixUtilExtension -cultures:en-us -out $msi $wixobj
    if ($LASTEXITCODE -ne 0) { throw "light fuer $arch fehlgeschlagen." }

    # Die einzelne EXE zusaetzlich nach dist kopieren. Laeuft sie gerade, ist sie gesperrt:
    # Dann nur hinweisen - die MSI-Datei ist davon nicht betroffen.
    $copy = Join-Path $dist "ZEBack-$arch.exe"
    try {
        Copy-Item $exe $copy -Force -ErrorAction Stop
    } catch {
        Write-Warning "$copy ist geoeffnet und wurde nicht ersetzt. ZEBack schliessen und das Skript erneut starten."
    }
    Write-Host "Fertig: $msi"
}
