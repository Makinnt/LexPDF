# Arma dist/windows/ para el instalador (o el tarball de prueba).
# Uso: .\installer\windows\stage.ps1
# Requiere: Rust estable + Java 17+ (build.rs descarga LT solo la 1ª vez).
$ErrorActionPreference = 'Stop'
$Root = Resolve-Path (Join-Path $PSScriptRoot '../..')
Set-Location $Root

cargo build --release
New-Item -ItemType Directory -Force dist/windows | Out-Null
Copy-Item target/release/lexpdf.exe dist/windows/
Copy-Item assets/pdfium/pdfium.dll dist/windows/
# licencias (ventana "Acerca de")
Copy-Item assets/licenses dist/windows/licenses -Recurse -Force
if (Test-Path assets/lt/languagetool-server.jar) {
  Copy-Item assets/lt dist/windows/lt -Recurse -Force
} else {
  Write-Warning 'Sin assets/lt (¿falló la descarga en build?). El instalador pedirá Java igualmente.'
}
# JRE minimizado con jlink (~40-60MB). Orden: $JAVA_HOME, jlink del sistema
# (solo si es 17+); si no hay JDK, cae al JRE completo de Temurin 21.
$Modules = 'java.base,java.logging,java.xml,java.naming,java.management,java.sql,java.desktop,java.net.http,jdk.httpserver,jdk.unsupported'
function Find-Jlink {
  $cands = @()
  if ($env:JAVA_HOME) { $cands += Join-Path $env:JAVA_HOME 'bin/jlink.exe' }
  $cands += (Get-Command jlink -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source)
  foreach ($c in $cands) {
    if ($c -and (Test-Path $c)) {
      $v = & $c --version 2>$null | Select-Object -First 1
      if ($v -match '(\d+)') { if ([int]$Matches[1] -ge 17) { return $c } }
    }
  }
  return $null
}
if (-not (Test-Path dist/windows/jre/bin/java.exe)) {
  $jlink = Find-Jlink
  if ($jlink) {
    Write-Host 'Generando JRE con jlink...'
    & $jlink --compress=2 --strip-debug --no-header-files --no-man-pages `
      --add-modules $Modules --output dist/windows/jre
  } else {
    Write-Host 'Descargando Temurin JRE 21 x64...'
    $api = 'https://api.adoptium.net/v3/binary/latest/21/ga/windows/x64/jre/hotspot/normal/eclipse'
    Invoke-WebRequest $api -OutFile dist/temurin-jre.zip
    Expand-Archive dist/temurin-jre.zip -DestinationPath dist/tmp-jre -Force
    $inner = Get-ChildItem dist/tmp-jre | Select-Object -First 1
    Move-Item $inner.FullName dist/windows/jre -Force
    Remove-Item dist/tmp-jre -Recurse -Force
    Remove-Item dist/temurin-jre.zip -Force
  }
}
Write-Host 'OK: dist/windows listo para iscc installer/windows/lexpdf.iss'
