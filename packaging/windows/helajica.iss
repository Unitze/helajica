; The setup.exe for Helajica. Installs per user, under %LOCALAPPDATA%\Programs,
; so the in-app updater can swap helajica.exe without asking for admin rights.
; Build: iscc /DAppVersion=X.Y.Z /DExe=<helajica.exe> /DOutDir=<dir> helajica.iss

[Setup]
AppId={{9B3E5C1A-7F24-4E8B-A6D2-3C1F8E5B7A90}
AppName=Helajica
AppVersion={#AppVersion}
AppPublisher=Unitze
AppPublisherURL=https://github.com/Unitze/helajica
DefaultDirName={localappdata}\Programs\Helajica
DisableDirPage=auto
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutDir}
OutputBaseFilename=helajica-windows-x86_64-setup
SetupIconFile=..\..\assets\helajica.ico
UninstallDisplayIcon={app}\helajica.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ja"; MessagesFile: "compiler:Languages\Japanese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Exe}"; DestDir: "{app}"; DestName: "helajica.exe"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Helajica"; Filename: "{app}\helajica.exe"
Name: "{autodesktop}\Helajica"; Filename: "{app}\helajica.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\helajica.exe"; Description: "{cm:LaunchProgram,Helajica}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; What the updater leaves next to the exe while swapping it.
Type: files; Name: "{app}\helajica.exe.*"
