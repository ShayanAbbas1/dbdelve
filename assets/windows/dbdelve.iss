; Windows installer. Invoked from dev/package-windows.ps1, which passes the
; version, the staged zip directory, and the output path. The installer wraps
; that same tree: dbdelve.exe, the ODBC bridge beside it, and the font
; licences the OFL asks to travel with the binary.
;
; PrivilegesRequired=lowest plus OverridesAllowed=dialog is the AppData vs
; Program Files choice. "Just for me" lands in %LOCALAPPDATA%\Programs\DBDelve
; with no elevation. "All users" lands in Program Files and asks for admin.
; Connections and query history stay in %APPDATA%\dbdelve either way; uninstall
; leaves them there, the same as Linux's install.sh --uninstall.
;
; AppId is load-bearing: Inno Setup uses it to find a previous install, so
; changing it orphans the uninstaller and stacks a second copy. The GUID is
; this installer's, not a Windows identity the app itself reads.
;
;
; /UPDATE is DBDelve's own switch, passed only by the app updating itself
; (src/update/windows.rs), always with /VERYSILENT /SUPPRESSMSGBOXES
; /NOCLOSEAPPLICATIONS and the install's mode and folder. Windows will not
; overwrite or delete a running executable, only rename it, and the app stays
; open while this runs, so with /UPDATE the [Code] below moves dbdelve.exe,
; the ODBC bridge's executable and the plugins folder into a folder ending
; .dbdelve-old inside {app} before anything is installed, and moves them back
; if the install fails. That folder goes on the next run of this installer, on
; uninstall, and at the app's next start where it may write there, unless
; something in it could not be put back: then dbdelve-unrestored.txt in it
; lists where each file belongs, and only uninstalling clears it. Without
; /UPDATE nothing is moved: an ordinary run behaves as it always has.
;
; Defines required on the iscc command line:
;   MyAppVersion  from Cargo.toml
;   StageDir      the staged zip directory (absolute)
;   OutputDir     where the setup exe is written
;   OutputBase    filename without .exe

#ifndef MyAppVersion
  #error MyAppVersion must be defined (/DMyAppVersion=...)
#endif
#ifndef StageDir
  #error StageDir must be defined (/DStageDir=...)
#endif
#ifndef OutputDir
  #error OutputDir must be defined (/DOutputDir=...)
#endif
#ifndef OutputBase
  #error OutputBase must be defined (/DOutputBase=...)
#endif

#define MyAppName "DBDelve"
#define MyAppPublisher "Shayan Abbas"
#define MyAppURL "https://github.com/ShayanAbbas1/dbdelve"
#define MyAppExeName "dbdelve.exe"

[Setup]
AppId={{300CEAF2-D7A2-4D97-B94B-20ED16631AF4}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
AppCopyright=Copyright (C) 2026 Shayan Abbas and contributors
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
LicenseFile={#StageDir}\LICENSE
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBase}
SetupIconFile={#SourcePath}dbdelve.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
UninstallDisplayIcon={app}\{#MyAppExeName}
DisableProgramGroupPage=yes
; Copying over a running dbdelve.exe fails; ask to close it first.
CloseApplications=yes
RestartApplications=no
UsedUserAreasWarning=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
; Name the two destinations the dialog actually picks. {autopf} is Program
; Files when elevated and %LOCALAPPDATA%\Programs when not; the labels have
; to say that, or the dialog only talks about "users".
PrivilegesRequiredOverrideTitle=Choose install location
PrivilegesRequiredOverrideInstruction=Where should DBDelve be installed?
PrivilegesRequiredOverrideCurrentUser=&Just for me (AppData, no administrator)
PrivilegesRequiredOverrideCurrentUserRecommended=&Just for me (AppData, no administrator) [recommended]
PrivilegesRequiredOverrideAllUsers=&All users (Program Files, requires administrator)
PrivilegesRequiredOverrideAllUsersRecommended=&All users (Program Files, requires administrator) [recommended]

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[InstallDelete]
; An upgrade only adds and overwrites. A plugin a later release drops or
; renames would otherwise linger here, and discover() prefers this directory.
Type: filesandordirs; Name: "{app}\plugins"

[UninstallDelete]
Type: filesandordirs; Name: "{app}\*.dbdelve-old"

[Files]
Source: "{#StageDir}\dbdelve.exe"; DestDir: "{app}"; Flags: ignoreversion
; Beside the executable, where db::plugin::discover() looks.
Source: "{#StageDir}\plugins\*"; DestDir: "{app}\plugins"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#StageDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[Code]
const
  SetAsideSuffix = '.dbdelve-old';
  { The same as UNRESTORED in src/update/windows.rs. }
  UnrestoredMarker = 'dbdelve-unrestored.txt';

var
  SetAsideDir: String;
  SetAsideFrom: TArrayOfString;
  SetAsideTo: TArrayOfString;
  InstallDone: Boolean;

function IsUpdate: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/UPDATE') = 0 then
      Result := True;
end;

{ Best effort: what still runs from an earlier update stays until next time. }
procedure ClearSetAside;
var
  FindRec: TFindRec;
  Path: String;
begin
  if FindFirst(ExpandConstant('{app}\*') + SetAsideSuffix, FindRec) then
    try
      repeat
        Path := ExpandConstant('{app}\') + FindRec.Name;
        if FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY = 0 then
          DeleteFile(Path)
        else if FileExists(AddBackslash(Path) + UnrestoredMarker) then
          Log('Kept ' + Path + ': it holds what a failed update could not put back')
        else
          DelTree(Path, True, True, True);
      until not FindNext(FindRec);
    finally
      FindClose(FindRec);
    end;
end;

function SetAside(const Name, Aside: String): Boolean;
var
  From, Dest: String;
  Count: Integer;
begin
  Result := True;
  From := ExpandConstant('{app}\') + Name;
  if not FileExists(From) and not DirExists(From) then
    Exit;
  Dest := AddBackslash(Aside) + ExtractFileName(Name);
  if not RenameFile(From, Dest) then begin
    Log('Could not move ' + From + ' aside to ' + Dest);
    Result := False;
    Exit;
  end;
  Count := GetArrayLength(SetAsideFrom);
  SetArrayLength(SetAsideFrom, Count + 1);
  SetArrayLength(SetAsideTo, Count + 1);
  SetAsideFrom[Count] := From;
  SetAsideTo[Count] := Dest;
end;

{ In reverse, so the plugins folder is back before the bridge returns into
  it. Whatever a failed install left in the way goes first. What will not go
  back stays listed, for DeinitializeSetup to try again, and the marker keeps
  every later clearing away from it. }
procedure PutBack;
var
  I, Count: Integer;
  KeptFrom, KeptTo: TArrayOfString;
  Listed: String;
begin
  Count := 0;
  Listed := '';
  for I := GetArrayLength(SetAsideFrom) - 1 downto 0 do begin
    if DirExists(SetAsideFrom[I]) then
      DelTree(SetAsideFrom[I], True, True, True)
    else if FileExists(SetAsideFrom[I]) then
      DeleteFile(SetAsideFrom[I]);
    ForceDirectories(ExtractFileDir(SetAsideFrom[I]));
    if RenameFile(SetAsideTo[I], SetAsideFrom[I]) then
      Log('Put back ' + SetAsideFrom[I])
    else begin
      Log('Could not put back ' + SetAsideFrom[I] + ' from ' + SetAsideTo[I]);
      SetArrayLength(KeptFrom, Count + 1);
      SetArrayLength(KeptTo, Count + 1);
      KeptFrom[Count] := SetAsideFrom[I];
      KeptTo[Count] := SetAsideTo[I];
      Count := Count + 1;
      Listed := Listed + SetAsideFrom[I] + ' <- ' + SetAsideTo[I] + #13#10;
    end;
  end;
  { Back in the order they were set aside, so a retry still runs in reverse. }
  SetArrayLength(SetAsideFrom, Count);
  SetArrayLength(SetAsideTo, Count);
  for I := 0 to Count - 1 do begin
    SetAsideFrom[I] := KeptFrom[Count - 1 - I];
    SetAsideTo[I] := KeptTo[Count - 1 - I];
  end;
  if SetAsideDir = '' then
    Exit;
  if Count > 0 then begin
    if not SaveStringToFile(AddBackslash(SetAsideDir) + UnrestoredMarker, Listed, False) then
      Log('Could not mark ' + SetAsideDir + ' as holding what was not put back');
  end else
    DeleteFile(AddBackslash(SetAsideDir) + UnrestoredMarker);
end;

{ Runs before Setup looks for files in use and before [InstallDelete]. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Aside: String;
begin
  Result := '';
  ClearSetAside;
  if not IsUpdate then
    Exit;
  Aside := ExpandConstant('{app}\') + GetDateTimeString('yyyymmddhhnnsszzz', #0, #0) + SetAsideSuffix;
  if not CreateDir(Aside) then begin
    Result := 'Could not create ' + Aside + '.';
    Exit;
  end;
  SetAsideDir := Aside;
  if not SetAside('dbdelve.exe', Aside) then begin
    PutBack;
    Result := 'Could not move the running DBDelve aside.';
    Exit;
  end;
  if not SetAside('plugins\odbc\bin\dbdelve-odbc.exe', Aside) then begin
    PutBack;
    Result := 'Could not move the running ODBC bridge aside.';
    Exit;
  end;
  { Whole, so a failed update can put it back. If it will not move,
    [InstallDelete] removes it as it always has. }
  SetAside('plugins', Aside);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then begin
    InstallDone := True;
    ClearSetAside;
  end;
end;

procedure DeinitializeSetup;
begin
  if not InstallDone then
    PutBack;
end;
