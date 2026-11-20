; ferrofs installer (SPEC §6), built with Inno Setup 6 by scripts\build-installer.ps1:
;   ISCC.exe /DAppVersion=0.1.0 /DBinDir=..\target\release /DOutputDir=..\target\installer ferrofs.iss
; Installs the four binaries for all users, registers the indexing service through
; `ferro-service install` (the same code as the command line), and optionally starts the
; search and link apps at sign-in. Uninstall stops and removes the service and asks whether
; to delete the index snapshots and service logs. Per-user settings, logs and the action
; log (needed to undo storage actions) are kept.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir "..\target\installer"
#endif

[Setup]
AppId={{7E0C2B1A-5F3D-4C8E-9A61-2D4B8F0E6C13}
AppName=ferrofs
AppVersion={#AppVersion}
AppVerName=ferrofs {#AppVersion}
AppPublisher=ferrofs
AppPublisherURL=https://github.com/yusufcanzdemir/ferrofs
AppSupportURL=https://github.com/yusufcanzdemir/ferrofs/issues
DefaultDirName={autopf}\ferrofs
DefaultGroupName=ferrofs
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 1809 or later.
MinVersion=10.0.17763
OutputDir={#OutputDir}
OutputBaseFilename=ferrofs-setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
LicenseFile=..\LICENSE
UninstallDisplayIcon={app}\ferro-storage.exe
UninstallDisplayName=ferrofs {#AppVersion}
CloseApplications=yes
RestartApplications=no
SetupLogging=yes
; The autostart values are per user by design (like ferro-link's own toggle); they are
; written for the user who runs the installer.
UsedUserAreasWarning=no

[Tasks]
Name: "autostartsearch"; Description: "Start ferrofs search at sign-in (Ctrl+Alt+Space opens it)"; GroupDescription: "Start with Windows:"
Name: "autostartlink"; Description: "Start ferrofs link at sign-in (Ctrl+B / Ctrl+G create links)"; GroupDescription: "Start with Windows:"
Name: "desktopicon"; Description: "Desktop shortcut for ferrofs storage"; GroupDescription: "Shortcuts:"

[Files]
Source: "{#BinDir}\ferro-service.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\ferro-search.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\ferro-storage.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\ferro-link.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\ferrofs search"; Filename: "{app}\ferro-search.exe"; Parameters: "--show"; Comment: "Instant file search"
Name: "{autoprograms}\ferrofs storage"; Filename: "{app}\ferro-storage.exe"; Comment: "Folder sizes, duplicates, cleanup and compression"
Name: "{autoprograms}\ferrofs link"; Filename: "{app}\ferro-link.exe"; Comment: "Create symlinks and hard links with Ctrl+B / Ctrl+G"
Name: "{autodesktop}\ferrofs storage"; Filename: "{app}\ferro-storage.exe"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ferrofs-search"; ValueData: """{app}\ferro-search.exe"""; Tasks: autostartsearch; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ferrofs-link"; ValueData: """{app}\ferro-link.exe"""; Tasks: autostartlink; Flags: uninsdeletevalue

[Run]
Filename: "{app}\ferro-search.exe"; Description: "Start ferrofs search now"; Flags: nowait postinstall runasoriginaluser skipifsilent
Filename: "{app}\ferro-link.exe"; Description: "Start ferrofs link now"; Flags: nowait postinstall runasoriginaluser skipifsilent

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';

var
  ServiceError: String;

{ Runs a program hidden and waits; True on exit code 0. }
function RunHidden(const FileName, Params: String): Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec(FileName, Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
  Log(Format('%s %s -> %d', [FileName, Params, ResultCode]));
end;

{ The background apps keep their executables open; they hold no unsaved state. }
procedure StopBackgroundApps(IncludeStorage: Boolean);
begin
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM ferro-search.exe');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM ferro-link.exe');
  if IncludeStorage then
    RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM ferro-storage.exe');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  { Stop and remove an existing service (an older version, or one registered from a
    development build) with the new binary, so its executable can be replaced and the
    service is registered again at the new path. Fails harmlessly when there is none. }
  ExtractTemporaryFile('ferro-service.exe');
  RunHidden(ExpandConstant('{tmp}\ferro-service.exe'), 'uninstall');
  StopBackgroundApps(False);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    { Refresh the shell's icon cache: shortcuts kept from an older version would
      otherwise show the old (or a blank) icon until the next sign-in. }
    RunHidden(ExpandConstant('{sys}\ie4uinit.exe'), '-show');
    if not RunHidden(ExpandConstant('{app}\ferro-service.exe'), 'install') then
    begin
      ServiceError := 'The ferrofs indexing service could not be installed or started. ' +
        'Search and storage tools need it. Details are in the setup log; you can retry from an ' +
        'elevated command prompt with:' + #13#10 + '"' + ExpandConstant('{app}') +
        '\ferro-service.exe" install';
      Log(ServiceError);
      if not WizardSilent then
        MsgBox(ServiceError, mbError, MB_OK);
    end;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Data: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    RunHidden(ExpandConstant('{app}\ferro-service.exe'), 'uninstall');
    StopBackgroundApps(True);
  end;
  if CurUninstallStep = usPostUninstall then
  begin
    { ferro-link's own "start with Windows" toggle may have written this too. }
    RegDeleteValue(HKCU, RunKey, 'ferrofs-search');
    RegDeleteValue(HKCU, RunKey, 'ferrofs-link');
    Data := ExpandConstant('{commonappdata}\ferrofs');
    if DirExists(Data) then
    begin
      { Snapshots are only a cache (the service rebuilds them); silent uninstalls remove them. }
      if UninstallSilent or (MsgBox('Also delete the index snapshots and service logs?' + #13#10 +
          Data + #13#10#13#10 + 'Your settings and the action log in your user profile are kept.',
          mbConfirmation, MB_YESNO) = IDYES) then
        DelTree(Data, True, True, True);
    end;
  end;
end;
