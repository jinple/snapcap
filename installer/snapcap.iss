; SnapCap installer (Inno Setup 6). Build with scripts\release.ps1, or directly:
;   ISCC.exe /DAppVersion=0.1.0 installer\snapcap.iss
; Expects `cargo build --release` to have produced snapcap.exe and snapcap_hook.dll.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
; Never change AppId: it's how upgrades and the uninstaller recognise SnapCap.
AppId={{6E1D5A3C-2B7F-4F0E-9C48-7A15D0B3E9A2}
AppName=SnapCap
AppVersion={#AppVersion}
AppVerName=SnapCap {#AppVersion}
AppPublisher=jinple
AppPublisherURL=https://github.com/jinple/snapcap
AppSupportURL=https://github.com/jinple/snapcap/issues
AppUpdatesURL=https://github.com/jinple/snapcap/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\SnapCap
DefaultGroupName=SnapCap
DisableProgramGroupPage=yes
; Per-user install by default (no admin prompt); the wizard offers "all users" too.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=..\dist
OutputBaseFilename=SnapCap-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\snapcap.exe
; If SnapCap is running during an upgrade, close it (and restart it afterwards).
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked
Name: "startup"; Description: "Start SnapCap when I sign in to Windows"; Flags: unchecked

[Files]
Source: "..\target\release\snapcap.exe"; DestDir: "{app}"; Flags: ignoreversion
; The optional in-game overlay hook. It must stay next to snapcap.exe.
Source: "..\target\release\snapcap_hook.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\SnapCap"; Filename: "{app}\snapcap.exe"
Name: "{autodesktop}\SnapCap"; Filename: "{app}\snapcap.exe"; Tasks: desktopicon
; Launches minimized to the taskbar so it doesn't pop a window up at every sign-in.
Name: "{userstartup}\SnapCap"; Filename: "{app}\snapcap.exe"; Parameters: "--minimized"; Tasks: startup

[Run]
Filename: "{app}\snapcap.exe"; Description: "Launch SnapCap"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Make sure it isn't running (and holding its files) while we uninstall.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM snapcap.exe"; Flags: runhidden; RunOnceId: "StopSnapCap"

[UninstallDelete]
; Settings live in %APPDATA%\SnapCap and are deliberately kept across uninstalls.
Type: filesandordirs; Name: "{app}"
