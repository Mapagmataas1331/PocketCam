#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif

#define MyAppName "PocketCam"
#define MyAppPublisher "PocketCam"
#define MyAppURL "https://github.com/Mapagmataas1331/PocketCam"
#define MyAppExeName "pocketcam.exe"
#define MyAppClsid "{{7B89B92E-FE71-42D0-8A41-E137D06EA184}"

[Setup]
AppId={{8F3A2C91-6B47-4D1E-9A50-C2E8F17B4D63}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}/releases
AppCopyright=Copyright (C) 2026 PocketCam contributors
DefaultDirName={autopf}\{#MyAppName}
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
InfoBeforeFile=info-before.txt
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
OutputDir=Output
OutputBaseFilename=PocketCamSetup
SetupIconFile=pocketcam.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
UsedUserAreasWarning=no
AppMutex=Local\PocketCam.single
SetupMutex=Local\PocketCam.setup
VersionInfoVersion={#MyAppVersion}
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}
VersionInfoDescription=Phone browser as a Windows webcam

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "..\target\release\pocketcam.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\redist\VirtualCameraMediaSource.dll"; DestDir: "{app}"; Flags: ignoreversion restartreplace
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\THIRD_PARTY.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "install-hooks.ps1"; DestDir: "{app}"; Flags: ignoreversion

[InstallDelete]
Type: files; Name: "{commonappdata}\PocketCam\VirtualCameraMediaSource.dll"

[Dirs]
Name: "{commonappdata}\PocketCam"

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"

[Registry]
Root: HKLM; Subkey: "SOFTWARE\Classes\CLSID\{#MyAppClsid}"; Flags: uninsdeletekey
Root: HKLM; Subkey: "SOFTWARE\Classes\CLSID\{#MyAppClsid}\InProcServer32"; ValueType: string; ValueName: ""; ValueData: "{app}\VirtualCameraMediaSource.dll"; Flags: uninsdeletekey
Root: HKLM; Subkey: "SOFTWARE\Classes\CLSID\{#MyAppClsid}\InProcServer32"; ValueType: string; ValueName: "ThreadingModel"; ValueData: "Both"

[Run]
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\install-hooks.ps1"" -Mode install -AppDir ""{app}"""; StatusMsg: "Registering camera folder and firewall rule..."; Flags: runhidden waituntilterminated
Filename: "{app}\{#MyAppExeName}"; Description: "Open PocketCam"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\install-hooks.ps1"" -Mode uninstall"; RunOnceId: "Firewall"; Flags: runhidden waituntilterminated
