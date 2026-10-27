; Instalador Windows de LexPDF.
; Se compila en CI (ver ../../.github/workflows/windows.yml) o a mano:
;   1. Arma dist/windows/ con stage.ps1
;   2. iscc installer/windows/lexpdf.iss
; La firma la pone SignPath en CI (ver README.md de esta carpeta).

#define MyAppName "LexPDF"
#define MyAppName "LexPDF"
#ifndef MyAppVersion
  #define MyAppVersion "0.2.5"
#endif
#define MyAppPublisher "LexiSec"
#define StageDir SourcePath + "/../../dist/windows"

[Setup]
AppId={{d318d4c9-5eb4-4837-9a16-654a20e5df26}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
; Ruta corta a propósito: LT tiene paths largos al descomprimir
DefaultGroupName={#MyAppName}
OutputDir={#SourcePath}/../../dist
OutputBaseFilename=lexpdf-setup
LicenseFile={#SourcePath}/../../LICENSE
SetupIconFile={#SourcePath}/lexpdf.ico
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
WizardStyle=modern
; Descomenta al tener SignPath configurado (firma el instalador):
; SignTool=signpath $f

[Languages]
Name: "spanish"; MessagesFile: "compiler:Languages/Spanish.isl"

[Tasks]
Name: "desktopicon"; Description: "Crear icono en el escritorio"; GroupDescription: "Accesos directos:"

[Files]
Source: "{#StageDir}\lexpdf.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\pdfium.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist
Source: "{#StageDir}\lt\*"; DestDir: "{app}\lt"; Flags: ignoreversion recursesubdirs createallsubdirs
; JRE empaquetado (opcional: si no está, el instalador pedirá Java, ver [Code])
Source: "{#StageDir}\jre\*"; DestDir: "{app}\jre"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\lexpdf.exe"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\lexpdf.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\lexpdf.exe"; Description: "Abrir {#MyAppName}"; Flags: nowait postinstall skipifsilent

[Code]
const
  TemurinURL = 'https://adoptium.net/temurin/releases/?version=21&os=windows&arch=x64';

function JavaSistema(): Boolean;
var
  Codigo: Integer;
begin
  // Solo Java del sistema: aqui APP aún no existe, no usarlo.
  Result := Exec('java.exe', '-version', '', SW_HIDE, ewWaitUntilTerminated, Codigo)
    and (Codigo = 0);
end;

function InitializeSetup(): Boolean;
begin
  Result := True;
  if JavaSistema() then
    Exit;
  { El instalador trae su propio JRE, esto es solo informativo. }
  if MsgBox(
    'No se detectó Java en el sistema.' + #13#10 +
    'El instalador incluye su propio Java, no necesitas hacer nada.' + #13#10 + #13#10 +
    '¿Continuar con la instalación?',
    mbInformation, MB_OKCANCEL) = IDCANCEL then
    Result := False;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Bundled, Msg: String;
  Codigo: Integer;
begin
  // Verificación post-instalación (aqui APP ya existe).
  if CurStep <> ssPostInstall then
    Exit;
  Bundled := ExpandConstant('{app}\jre\bin\java.exe');
  if FileExists(Bundled) or JavaSistema() then
    Exit;
  Msg := 'Aviso: no se encontró Java (ni incluido ni del sistema).' + #13#10 +
    'La revisión no funcionará hasta instalar Java 17+.' + #13#10 + #13#10 +
    '¿Abrir la página de descarga de Temurin ahora?';
  if MsgBox(Msg, mbError, MB_YESNO) = IDYES then
    ShellExec('open', TemurinURL, '', '', SW_SHOW, ewNoWait, Codigo);
end;
