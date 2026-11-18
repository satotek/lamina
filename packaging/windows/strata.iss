; Inno Setup script for the Strata installer.
;
; Run through packaging/windows/package.ps1 -Installer, which stages the
; portable zip's files into a directory and passes the version, the staging
; directory and the output name on the iscc command line, so the installer
; and the zip always carry the same bits. Inno Setup 6.3+ is required for the
; x64compatible architecture identifier.

#ifndef AppVersion
  #error AppVersion must be passed on the command line (/DAppVersion=...)
#endif

#ifndef StageDir
  #error StageDir must be passed on the command line (/DStageDir=...)
#endif

[Setup]
; Fixed for the lifetime of the app: an upgrade replaces this install in
; place instead of leaving a second entry in Apps & features.
AppId={{6F1C2B8E-5A47-4E3D-9B21-7C0D3E8A4F15}
AppName=Strata
AppVersion={#AppVersion}
AppPublisher=satotek
; A per-user install needs no elevation and keeps every file inside the
; user's own profile.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\Strata
DisableDirPage=yes
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Restart Manager closes a running Strata during an upgrade; whether it is
; relaunched is decided by the [Run] section below, so Inno must not do it.
CloseApplications=yes
RestartApplications=no
SolidCompression=yes
; Shortcuts and Apps & features take their icon from strata.exe, which embeds
; resources/icon/strata.ico at build time; setup.exe itself reuses that
; source icon. Relative paths resolve against this script's directory.
SetupIconFile=..\..\crates\strata-app\resources\icon\strata.ico
UninstallDisplayName=Strata
UninstallDisplayIcon={app}\strata.exe
; The app is dual-licensed (MIT OR Apache-2.0), which does not fit Inno's
; single-file license page; both texts are installed next to strata.exe.

[Languages]
; The app itself ships English and Japanese, so the wizard offers both.
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ja"; MessagesFile: "compiler:Languages\Japanese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; A wildcard over the staged zip contents: whatever package.ps1 stages is
; what both artifacts ship, so the file list lives in one place only.
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Strata"; Filename: "{app}\strata.exe"
Name: "{autodesktop}\Strata"; Filename: "{app}\strata.exe"; Tasks: desktopicon

[Run]
; Interactive installs keep the usual postinstall launch checkbox.
Filename: "{app}\strata.exe"; Description: "{cm:LaunchProgram,Strata}"; Flags: nowait postinstall skipifsilent
; A silent run is the in-app updater, which closes Strata so this setup can
; replace it, so the installer relaunches the app itself instead.
Filename: "{app}\strata.exe"; Flags: nowait; Check: WizardSilent

[UninstallDelete]
; The marker only ever belongs to an installed copy; it must not survive
; the uninstaller.
Type: files; Name: "{app}\installed-by-setup"

[Code]
const
  InstalledBySetup = 'installed-by-setup';

procedure CurStepChanged(CurStep: TSetupStep);
begin
  // The in-app updater offers to update through this installer only when it
  // finds this empty marker next to strata.exe. It is written here instead
  // of being shipped as a file, so an unpacked portable zip never claims
  // installer-managed updates.
  if CurStep = ssPostInstall then
    SaveStringToFile(ExpandConstant('{app}\' + InstalledBySetup), '', False);
end;
