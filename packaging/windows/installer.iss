; Inno Setup script. Build with:
;   iscc /DAppVersion=0.1.0 packaging\windows\installer.iss
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
AppId={{6F1C3B52-8D0A-4F43-9A3B-2C1E5A7D9B10}
AppName=Android Monitor
AppVersion={#AppVersion}
AppPublisher=Android Monitor
DefaultDirName={autopf}\Android Monitor
DefaultGroupName=Android Monitor
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\android-monitor.exe
UninstallDisplayName=Android Monitor
SetupIconFile=..\..\assets\icon.ico
OutputDir=..\..\dist
OutputBaseFilename=android-monitor-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
LicenseFile=..\..\LICENSE

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked
Name: "addtopath"; Description: "Add to &PATH (to run `android-monitor --mcp-stdio` from MCP clients)"; GroupDescription: "Command line:"; Flags: unchecked

[Files]
Source: "..\..\target\release\android-monitor.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Android Monitor"; Filename: "{app}\android-monitor.exe"
Name: "{autodesktop}\Android Monitor"; Filename: "{app}\android-monitor.exe"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NeedsPath(ExpandConstant('{app}'))

[Run]
Filename: "{app}\android-monitor.exe"; Description: "Launch Android Monitor"; Flags: nowait postinstall skipifsilent

[Code]
function NeedsPath(Dir: string): Boolean;
var Paths: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths) then
    Paths := '';
  Result := Pos(';' + Lowercase(Dir) + ';', ';' + Lowercase(Paths) + ';') = 0;
end;

function InitializeSetup(): Boolean;
begin
  Result := True;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpFinished then
    Log('Reminder: Android Monitor needs adb (Android platform-tools) on PATH.');
end;
