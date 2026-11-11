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
;
; When Git is not found, the wizard offers (ticked) to download the latest
; Git for Windows from its GitHub releases, check it against the SHA-256
; GitHub publishes for it, and install it silently for the current user
; (into %LOCALAPPDATA%\Programs\Git, no administrator rights). Silent runs,
; updates among them, never do that unless asked with /INSTALLGIT=1. A
; failed download or install never stops GitKrill from installing: the app
; offers Git again when it starts (src/git_setup.rs).

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
Name: "installgit"; Description: "Download and install Git for Windows, for my account only (about 65 MB)"; GroupDescription: "Git was not found on this computer. GitKrill needs it to commit, pull and push:"; Check: OfferGit

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "gitkrill.exe"; Flags: ignoreversion
Source: "gitkrill-installed"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\GitKrill"; Filename: "{app}\gitkrill.exe"
Name: "{autodesktop}\GitKrill"; Filename: "{app}\gitkrill.exe"; Tasks: desktopicon

[Registry]
; System notifications are shown under this application id (see
; src/notification.rs). GitKrill writes its name and icon there itself,
; which also covers the portable download; uninstalling removes the key.
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\Moulman.GitKrill"; ValueType: string; ValueName: "DisplayName"; ValueData: "GitKrill"; Flags: uninsdeletekey

[Run]
; After installing by hand: offer to start it.
Filename: "{app}\gitkrill.exe"; Description: "{cm:LaunchProgram,GitKrill}"; Flags: nowait postinstall skipifsilent
; After an update asked for with "Restart to update".
Filename: "{app}\gitkrill.exe"; Flags: nowait; Check: Relaunch

[Code]
const
  GitRelease = 'https://api.github.com/repos/git-for-windows/git/releases/latest';
  GitDownloads = 'https://github.com/git-for-windows/git/releases/download/';

var
  DownloadPage: TDownloadWizardPage;
  GitChecked, GitWasFound, GitDownloaded: Boolean;

function Relaunch: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:relaunch|0}') = '1');
end;

{ Whether git is on PATH or where Git for Windows installs itself. }
function GitFound: Boolean;
var
  Dir: String;
begin
  if not GitChecked then begin
    GitChecked := True;
    GitWasFound := (FileSearch('git.exe', GetEnv('PATH')) <> '')
      or FileExists(ExpandConstant('{userpf}\Git\cmd\git.exe'))
      or FileExists(ExpandConstant('{commonpf64}\Git\cmd\git.exe'))
      or (RegQueryStringValue(HKCU, 'Software\GitForWindows', 'InstallPath', Dir) and FileExists(AddBackslash(Dir) + 'cmd\git.exe'))
      or (RegQueryStringValue(HKLM, 'Software\GitForWindows', 'InstallPath', Dir) and FileExists(AddBackslash(Dir) + 'cmd\git.exe'));
    Log('Git found: ' + IntToStr(Ord(GitWasFound)));
  end;
  Result := GitWasFound;
end;

{ The "install Git" task, shown only when there is none, and never in a
  silent run (an update must not start a 65 MB download by itself). }
function OfferGit: Boolean;
begin
  Result := (not WizardSilent) and (not GitFound);
end;

function WantGit: Boolean;
begin
  if WizardSilent then
    Result := (ExpandConstant('{param:installgit|0}') = '1') and (not GitFound)
  else
    Result := WizardIsTaskSelected('installgit');
end;

{ Only letters, digits, dots, dashes and underscores: safe in an address
  and a file name. }
function IsPlain(const S: String): Boolean;
var
  I: Integer;
  C: Char;
begin
  Result := Length(S) > 0;
  for I := 1 to Length(S) do begin
    C := S[I];
    if not (((C >= 'a') and (C <= 'z')) or ((C >= 'A') and (C <= 'Z')) or ((C >= '0') and (C <= '9'))
            or (C = '.') or (C = '-') or (C = '_')) then
      Result := False;
  end;
end;

function IsSha256(const S: String): Boolean;
var
  I: Integer;
begin
  Result := Length(S) = 64;
  for I := 1 to Length(S) do
    if not (((S[I] >= '0') and (S[I] <= '9')) or ((S[I] >= 'a') and (S[I] <= 'f'))) then
      Result := False;
end;

{ The text between Prefix (the first one at or after Start) and the next
  double quote. At becomes the position just after that quote, or 0 when
  Prefix is not there. }
function ValueAfter(const S, Prefix: String; Start: Integer; var Value: String; var At: Integer): Boolean;
var
  Rest: String;
  P, Q: Integer;
begin
  Result := False;
  At := 0;
  Rest := Copy(S, Start, Length(S));
  P := Pos(Prefix, Rest);
  if P = 0 then
    Exit;
  Rest := Copy(Rest, P + Length(Prefix), Length(Rest));
  Q := Pos('"', Rest);
  if Q = 0 then
    Exit;
  Value := Copy(Rest, 1, Q - 1);
  At := Start + P - 1 + Length(Prefix) + Q;
  Result := True;
end;

{ Reads GitHub's description of the latest Git for Windows release: the
  installer for this processor and its SHA-256. Each file is described by
  its name, then (among others) its digest, before the next file's name. }
function FindGitInstaller(const Json: String; var Url, Sha: String): Boolean;
var
  Text, Tag, Name, Suffix, Digest, Ignored: String;
  At, Next, DigestAt, NextName: Integer;
begin
  Result := False;
  Text := Json;
  StringChangeEx(Text, '": "', '":"', True);
  if (not ValueAfter(Text, '"tag_name":"', 1, Tag, At)) or (not IsPlain(Tag)) then
    Exit;
  if IsArm64 then
    Suffix := '-arm64.exe'
  else
    Suffix := '-64-bit.exe';
  At := 1;
  while ValueAfter(Text, '"name":"', At, Name, Next) do begin
    At := Next;
    if (Pos('Git-', Name) = 1) and IsPlain(Name)
       and (Copy(Name, Length(Name) - Length(Suffix) + 1, Length(Suffix)) = Suffix) then begin
      if ValueAfter(Text, '"digest":"sha256:', At, Digest, DigestAt) then begin
        if not ValueAfter(Text, '"name":"', At, Ignored, NextName) then
          NextName := Length(Text) + 1;
        Digest := Lowercase(Digest);
        if (DigestAt < NextName) and IsSha256(Digest) then begin
          Url := GitDownloads + Tag + '/' + Name;
          Sha := Digest;
          Result := True;
        end;
      end;
      Exit;
    end;
  end;
end;

function OnDownloadProgress(const Url, FileName: String; const Progress, ProgressMax: Int64): Boolean;
begin
  Result := True;
end;

procedure InitializeWizard;
begin
  DownloadPage := CreateDownloadPage('Downloading Git for Windows',
    'GitKrill needs Git. Setup is downloading it from its GitHub releases.', @OnDownloadProgress);
  DownloadPage.ShowBaseNameInsteadOfUrl := True;
end;

// Fetches the installer into the temporary folder, checked against its
// SHA-256. A failure is reported and otherwise ignored: GitKrill installs
// either way.
procedure DownloadGit;
var
  Json: AnsiString;
  Url, Sha: String;
begin
  DownloadPage.Clear;
  DownloadPage.Add(GitRelease, 'git-release.json', '');
  DownloadPage.Show;
  try
    try
      DownloadPage.Download;
      if (not LoadStringFromFile(ExpandConstant('{tmp}\git-release.json'), Json))
         or (not FindGitInstaller(String(Json), Url, Sha)) then
        RaiseException('its latest release could not be read');
      Log('Git for Windows: ' + Url + ' sha256 ' + Sha);
      DownloadPage.Clear;
      DownloadPage.Add(Url, 'Git-Setup.exe', Sha);
      DownloadPage.Download;
      GitDownloaded := True;
    except
      Log('Git for Windows was not downloaded: ' + GetExceptionMessage);
      if not DownloadPage.AbortedByUser then
        SuppressibleMsgBox('Git for Windows could not be downloaded: ' + AddPeriod(GetExceptionMessage) + #13#10#13#10
          + 'GitKrill is installed anyway, and offers to get Git when it starts.', mbError, MB_OK, IDOK);
    end;
  finally
    DownloadPage.Hide;
  end;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpReady) and WantGit then
    DownloadGit;
end;

{ Runs Git's installer without questions, for this user only, with Git's
  own default choices, once GitKrill's files are in place. }
procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
  Params, GitLog: String;
begin
  if (CurStep <> ssPostInstall) or (not GitDownloaded) then
    Exit;
  WizardForm.StatusLabel.Caption := 'Installing Git for Windows...';
  WizardForm.FilenameLabel.Caption := '';
  WizardForm.ProgressGauge.Style := npbstMarquee;
  GitLog := ExpandConstant('{%TEMP}\GitKrill-git-setup.log');
  Params := '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL /SP- /DIR="' + ExpandConstant('{userpf}\Git')
    + '" /LOG="' + GitLog + '"';
  if Exec(ExpandConstant('{tmp}\Git-Setup.exe'), Params, '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0) then
    Log('Git for Windows is installed')
  else begin
    Log('Git for Windows setup failed: ' + IntToStr(Code));
    SuppressibleMsgBox('Git for Windows could not be installed (code ' + IntToStr(Code) + '). Its log is ' + GitLog
      + #13#10#13#10 + 'GitKrill is installed, and offers to get Git when it starts.', mbError, MB_OK, IDOK);
  end;
  WizardForm.ProgressGauge.Style := npbstNormal;
end;
