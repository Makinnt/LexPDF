#!/bin/sh
# Arma dist/AppDir para el AppImage (o tarball de prueba).
# Uso: ./installer/linux/stage-appimage.sh
# Requiere: Rust estable + Java 17+ (build.rs descarga LT solo la primera vez).
set -eu
ROOT=$(dirname "$(dirname "$(dirname "$(readlink -f "$0")")")")
cd "$ROOT"

cargo build --release
APPDIR="$ROOT/dist/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" "$APPDIR/usr/share/lexpdf"

cp target/release/lexpdf "$APPDIR/usr/bin/"
cp assets/pdfium/libpdfium.so "$APPDIR/usr/lib/"
cp installer/linux/lexpdf.png "$APPDIR/"
cp installer/linux/lexpdf.desktop "$APPDIR/"
# licencias (ventana "Acerca de"):GPL-3, LGPL-2.1, Apache-2.0, MIT, BSD, iconos
cp -r assets/licenses "$APPDIR/usr/share/lexpdf/licenses"
if [ -d assets/lt ]; then
  cp -r assets/lt "$APPDIR/usr/share/lexpdf/lt"
else
  echo 'AVISO: sin assets/lt (¿falló la descarga en build?). El AppImage pedirá lt/ igualmente.' >&2
fi

# JRE minimizado con jlink (~40-60MB). Orden: $JAVA_HOME, jlink del sistema
# (solo si es 17+; en CI el runner trae un 11 que NO sirve para LT), o nada.
pick_jlink() {
  sys=$(command -v jlink 2>/dev/null || true)
  for c in "${JAVA_HOME:-}/bin/jlink" "$sys"; do
    [ -n "$c" ] && [ -x "$c" ] || continue
    v=$("$c" --version 2>/dev/null | grep -o '[0-9]\+' | head -n 1)
    if [ "${v:-0}" -ge 17 ]; then echo "$c"; return 0; fi
  done
  return 1
}
if [ ! -x "$APPDIR/usr/share/lexpdf/jre/bin/java" ]; then
  if JLINK=$(pick_jlink); then
    "$JLINK" --compress=2 --strip-debug --no-header-files --no-man-pages \
      --add-modules java.base,java.logging,java.xml,java.naming,java.management,java.sql,java.desktop,java.net.http,jdk.httpserver,jdk.unsupported \
      --output "$APPDIR/usr/share/lexpdf/jre"
  fi
fi

cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export LD_LIBRARY_PATH="$HERE/usr/lib:${LD_LIBRARY_PATH:-}"
export PDFIUM_DYNAMIC_LIB_PATH="$HERE/usr/lib"
export LT_HOME="$HERE/usr/share/lexpdf/lt"
export JAVA_BIN="$HERE/usr/share/lexpdf/jre/bin/java"
exec "$HERE/usr/bin/lexpdf" "$@"
EOF
chmod +x "$APPDIR/AppRun"
"$APPDIR/usr/share/lexpdf/jre/bin/java" -version 2>&1 | head -n 1 || true
echo "OK: $APPDIR listo (AppRun + desktop + icono)"
