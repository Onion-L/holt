; Inno Setup script for the per-user Holt setup installer. Compiled by
; the Release workflow's two Windows jobs with
;
;   iscc /DVersion=<workspace version> /DArch=<x64|arm64> scripts\holt-setup.iss
;
; from the repository root; Version and Arch have no defaults on purpose —
; a compile without them fails instead of shipping a misnamed asset.
; Relative paths in this script resolve against *its own* directory (the
; Inno compiler's rule), hence the `..` back to the repo root, and the
; output lands there too, where the workflow's upload step expects it.
;
; The install is per-user (PrivilegesRequired=lowest): it writes only
; {localappdata}\Programs\Holt and the HKCU uninstall key — no HKLM, no
; administrator. It also drops holt-release.marker beside holt.exe, the
; file whose presence marks a directory the Windows self-updater may
; swap (crates/engine/src/update_platform.rs); the zip asset carries the
; same marker for the same reason.

#ifndef Version
  #error "Version must be injected: iscc /DVersion=0.7.0 ..."
#endif
#ifndef Arch
  #error "Arch must be injected: iscc /DArch=x64 or /DArch=arm64"
#endif

[Setup]
AppId={{F38ECF17-B322-4173-A268-DFE25316FA5E}
AppName=Holt
AppVersion={#Version}
; Never request elevation: everything this installer touches is per-user.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\Holt
DisableProgramGroupPage=yes
; The setup asset name is a contract — the download page matches it by
; the -<arch>-setup.exe suffix, and the manifest digests it by name.
OutputBaseFilename=Holt-{#Version}-{#Arch}-setup
; Relative to this script's directory (scripts/): the repo root, where
; the Release workflow's upload step runs from.
OutputDir=..
SolidCompression=yes
WizardStyle=modern
; The binaries are arch-specific, so keep each setup on its own machines
; (x64compatible covers x64 Windows plus x64 emulation on ARM64 Windows).
#if Arch == "arm64"
ArchitecturesAllowed=arm64
#else
ArchitecturesAllowed=x64compatible
#endif

[Files]
Source: "..\target\release\holt.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Holt"; Filename: "{app}\holt.exe"

[Code]
procedure CurStepChanged(CurStep: TSetupStep);
begin
  // Written at install time rather than packaged, so the version rides
  // in through the compile-time /DVersion define. The updater only
  // checks the marker's existence; its content is for humans.
  if CurStep = ssPostInstall then
    SaveStringToFile(ExpandConstant('{app}\holt-release.marker'), '{#Version}', False);
end;

[UninstallDelete]
; Not installed by [Files], so the uninstaller needs to be told.
Type: files; Name: "{app}\holt-release.marker"
