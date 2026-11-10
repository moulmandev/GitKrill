; The Windows installer, built by CI with Inno Setup 6:
;
;   iscc /DAppVersion=0.2.0 installer\gitkrill.iss
;
; It installs for the current user only, without administrator rights, into
; %LOCALAPPDATA%\Programs\GitKrill, with a Start menu entry. The marker file
; it leaves next to the program tells GitKrill to update itself by running
; the next release's installer (see src/update.rs), which GitKrill does with
;
;   GitKrill-Setup-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- [/RELAUNCH=1]
;
; /RELAUNCH=1 starts GitKrill again once the update is in place.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=x.y.z installer\gitkrill.iss
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\gitkrill.exe"
#endif

[Setup]
; Never change: Windows knows the installation by this id.
AppId={{6B1F0C3A-8E4D-4C52-9A77-2D5E9F1B0C84}
AppName=GitKrill
AppVersion={#AppVersion}
AppVerName=GitKrill {#AppVersion}
AppPublisher=Moulman
AppPublisherURL=https://github.com/moulmandev/GitKrill
AppSupportURL=https://github.com/moulmandev/GitKrill/issues
AppUpdatesURL=https://github.com/moulmandev/GitKrill/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\GitKrill
DisableProgramGroupPage=yes
DisableDirPage=yes
; Per user: no elevation prompt, and updates can run silently.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=GitKrill-Setup-x64
SetupIconFile=..\assets\gitkrill.ico
UninstallDisplayIcon={app}\gitkrill.exe
UninstallDisplayName=GitKrill
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; A running GitKrill is closed before its files are replaced (silently
; during an update), and not started again by Windows: /RELAUNCH does that.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "gitkrill.exe"; Flags: ignoreversion
Source: "gitkrill-installed"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\GitKrill"; Filename: "{app}\gitkrill.exe"
Name: "{autodesktop}\GitKrill"; Filename: "{app}\gitkrill.exe"; Tasks: desktopicon

[Run]
; After installing by hand: offer to start it.
Filename: "{app}\gitkrill.exe"; Description: "{cm:LaunchProgram,GitKrill}"; Flags: nowait postinstall skipifsilent
; After an update asked for with "Restart to update".
Filename: "{app}\gitkrill.exe"; Flags: nowait; Check: Relaunch

[Code]
function Relaunch: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:relaunch|0}') = '1');
end;
