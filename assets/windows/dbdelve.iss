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
