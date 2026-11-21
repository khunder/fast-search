; Windows installer for Fast search, built with Inno Setup 6:
;
;   cargo win
;   target\x86_64-pc-windows-msvc\release\ff-gui.exe --write-icon target\x86_64-pc-windows-msvc\release\fast-search.ico
;   iscc /DAppVersion=0.2.0 packaging\windows\fast-search.iss
;
; It installs ff.exe, ff-gui.exe and ffd.exe, adds the folder to PATH,
; puts "Fast search" in the Start menu with the Ctrl+Alt+F hot key and,
; when installed for all users, can install the ffd service.
;
; Installing for the current user needs no administrator rights; the
; setup offers both. Run again, it updates in place, keeping the folder,
; the choices made and the install mode. `ff update` runs it silently:
;   fast-search-windows-x86_64-setup.exe /SILENT [/STARTGUI=1]

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\..\target\x86_64-pc-windows-msvc\release"
#endif

[Setup]
AppId={{5B0E6C2A-3F4D-4E8B-9A71-2C6D8F1E0B47}
AppName=Fast search
AppVersion={#AppVersion}
AppVerName=Fast search {#AppVersion}
AppPublisher=Fast search
AppPublisherURL=https://github.com/khunder/fast-search
AppSupportURL=https://github.com/khunder/fast-search/issues
AppUpdatesURL=https://github.com/khunder/fast-search/releases
DefaultDirName={autopf}\Fast search
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
ChangesEnvironment=yes
CloseApplications=yes
RestartApplications=yes
LicenseFile=..\..\LICENSE
UninstallDisplayIcon={app}\ff-gui.exe
; The setup's own icon: written by `ff-gui --write-icon` (see above).
#if FileExists(AddBackslash(SourcePath) + BinDir + "\fast-search.ico")
SetupIconFile={#BinDir}\fast-search.ico
#endif
UninstallDisplayName=Fast search
OutputDir=..\..\dist
OutputBaseFilename=fast-search-windows-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: path; Description: "Add ff to the PATH (use it from any terminal)"
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked
Name: startup; Description: "Start at sign-in, in the notification area (Ctrl+Alt+F then opens it at once)"; Flags: unchecked
Name: service; Description: "Install the ffd service: keeps the index current, and reads NTFS drives directly (fast)"; Flags: unchecked; Check: IsAdminInstallMode

[Files]
Source: "{#BinDir}\ff.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\ff-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\ffd.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
; The same shortcut the window's Options create: it carries the hot key.
Name: "{autoprograms}\Fast search"; Filename: "{app}\ff-gui.exe"; WorkingDir: "{app}"; Comment: "Find files instantly by name"; HotKey: ctrl+alt+f
Name: "{autodesktop}\Fast search"; Filename: "{app}\ff-gui.exe"; WorkingDir: "{app}"; Comment: "Find files instantly by name"; Tasks: desktopicon

[Registry]
; The same entry as the window's Options > Desktop > Start at sign-in
; (settings.rs: RUN_VALUE).
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Fast search"; ValueData: """{app}\ff-gui.exe"" --hidden"; Tasks: startup

[Run]
Filename: "{app}\ffd.exe"; Parameters: "--install-service"; Flags: runhidden waituntilterminated; StatusMsg: "Starting the ffd service..."; Tasks: service
Filename: "{app}\ff-gui.exe"; Description: "Start Fast search"; Flags: nowait postinstall skipifsilent
; An update started from the window opens it again: /STARTGUI=1.
Filename: "{app}\ff-gui.exe"; Flags: nowait runasoriginaluser; Check: StartGuiAfterUpdate

[UninstallRun]
; Checks here are made at install time, before any service exists: so
; this runs for every all-users install, and ffd ignores a missing service.
Filename: "{app}\ffd.exe"; Parameters: "--uninstall-service"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"; Check: IsAdminInstallMode

[Code]
const
  UserEnv = 'Environment';
  MachineEnv = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

function EnvRoot: Integer;
begin
  if IsAdminInstallMode then Result := HKEY_LOCAL_MACHINE else Result := HKEY_CURRENT_USER;
end;

function EnvKey: String;
begin
  if IsAdminInstallMode then Result := MachineEnv else Result := UserEnv;
end;

{ Position of the install folder in a PATH value, 0 if absent. }
function PathPos(Path: String): Integer;
begin
  Result := Pos(';' + Uppercase(ExpandConstant('{app}')) + ';', ';' + Uppercase(Path) + ';');
end;

procedure AddToPath;
var
  Path: String;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then Path := '';
  if PathPos(Path) > 0 then exit;
  if (Path <> '') and (Path[Length(Path)] <> ';') then Path := Path + ';';
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Path + ExpandConstant('{app}'));
end;

procedure RemoveFromPath;
var
  Path: String;
  P: Integer;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then exit;
  P := PathPos(Path);
  if P = 0 then exit;
  Path := ';' + Path + ';';
  Delete(Path, P, Length(ExpandConstant('{app}')) + 1);
  Path := Copy(Path, 2, Length(Path) - 2);
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Path);
end;

function StartGuiAfterUpdate: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:STARTGUI|0}') = '1');
end;

function ServiceInstalled: Boolean;
begin
  Result := IsAdminInstallMode and RegKeyExists(HKEY_LOCAL_MACHINE, 'SYSTEM\CurrentControlSet\Services\ffd');
end;

{ A running service holds ffd.exe open: stop it before the files are
  replaced. The service task starts it again afterwards. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  if ServiceInstalled then
    { net waits until the service has stopped; sc would not. }
    Exec(ExpandConstant('{sys}\net.exe'), 'stop ffd', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('path') then AddToPath;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then begin
    RemoveFromPath;
    { Start at sign-in may have been set from the window, not by Setup. }
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'Fast search');
  end;
end;
