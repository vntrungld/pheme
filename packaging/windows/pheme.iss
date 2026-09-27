; Built by CI with:  iscc /DAppVersion=0.1.0 packaging\windows\pheme.iss
; AppVersion is required; the script refuses to compile without it rather
; than bake in a version that will quietly go stale.
#ifndef AppVersion
  #error AppVersion must be passed with /DAppVersion=x.y.z
#endif

[Setup]
AppId={{9C8F3B21-6E4A-4E5D-9C3A-7F2D5A1B8E40}
AppName=Pheme
AppVersion={#AppVersion}
AppPublisher=Lam Duc Trung
AppPublisherURL=https://github.com/vntrungld/pheme
DefaultDirName={autopf}\Pheme
DefaultGroupName=Pheme
DisableProgramGroupPage=yes
; No administrator. With PrivilegesRequired=lowest, {autopf} resolves to
; %LOCALAPPDATA%\Programs and no UAC prompt appears. Pheme creates no
; service and writes nothing outside the user's own profile, and an
; installer that asks for administrator teaches people to grant it.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Tells Windows to broadcast the environment change, so a terminal opened
; after the install sees the new PATH without a sign-out.
ChangesEnvironment=yes
OutputDir=Output
OutputBaseFilename=pheme-{#AppVersion}-x86_64-windows-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
LicenseFile=..\..\LICENSE

[Files]
Source: "..\..\target\release\pheme.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\docs\testing.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Pheme"; Filename: "{app}\pheme.exe"
Name: "{group}\Uninstall Pheme"; Filename: "{uninstallexe}"

[Tasks]
Name: "startup"; Description: "Start Pheme when I sign in"
Name: "addtopath"; Description: "Add Pheme to PATH (for pheme displays, pheme pair)"; Flags: unchecked

[Registry]
; The key Task Manager's Startup tab reads, so somebody who changes their
; mind finds the switch where they will look for it.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
    ValueType: string; ValueName: "Pheme"; ValueData: """{app}\pheme.exe"""; \
    Flags: uninsdeletevalue; Tasks: startup
; Guarded by NeedsAddPath so a repeat install appends once, not twice.
; Two entries with complementary checks rather than one: on a profile that
; has never had a user Path, {olddata} expands to nothing and a single
; entry would write ";C:\...", whose empty leading segment Windows resolves
; as the current directory.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{olddata};{app}"; Tasks: addtopath; \
    Check: NeedsAddPath(ExpandConstant('{app}')) and HasExistingPath
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{app}"; Tasks: addtopath; \
    Check: NeedsAddPath(ExpandConstant('{app}')) and not HasExistingPath

[Code]
const
  EnvironmentKey = 'Environment';

{ True when {app} is not already one of the user's PATH entries. The
  comparison pads both sides with ';' so the first and last entries match
  the same way every middle one does. }
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath) then
  begin
    Result := True;
    Exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

{ True when the user already has a non-empty Path, so a new entry needs a
  separator in front of it. Without this test, {olddata} expands to nothing
  on a profile that never had one and the value becomes ";C:\...", whose
  empty leading segment Windows resolves as the current directory. }
function HasExistingPath(): Boolean;
var
  OrigPath: string;
begin
  Result := RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath)
            and (OrigPath <> '');
end;

{ Take {app} back out of PATH without disturbing anything else in it. }
procedure RemovePath(Path: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Exit;
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Path) + ';', Uppercase(Paths));
  if P = 0 then
    Exit;
  { Delete the entry and the ';' that followed it, then the two sentinels. }
  Delete(Paths, P, Length(Path) + 1);
  Delete(Paths, 1, 1);
  if (Length(Paths) > 0) and (Paths[Length(Paths)] = ';') then
    Delete(Paths, Length(Paths), 1);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemovePath(ExpandConstant('{app}'));
end;
