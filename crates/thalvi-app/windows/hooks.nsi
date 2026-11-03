; Tauri's per-user default is %LOCALAPPDATA%\Thalvi, which is also where Thalvi keeps its data.
; A new install goes to %LOCALAPPDATA%\Programs\Thalvi instead; a chosen or existing directory stays.
!define THALVI_TEMPLATE_DEFAULT "$LOCALAPPDATA\Thalvi"

Function ThalviDefaultInstallDir
  ${If} $INSTDIR == "${THALVI_TEMPLATE_DEFAULT}"
  ${AndIfNot} ${FileExists} "$INSTDIR\uninstall.exe"
    StrCpy $INSTDIR "$LOCALAPPDATA\Programs\Thalvi"
  ${EndIf}
FunctionEnd

; The wizard shows the directory page after this, so the default it offers is the new one.
!define MUI_CUSTOMFUNCTION_GUIINIT ThalviDefaultInstallDir

; Silent installs skip the wizard, so they take the default here, before the installer creates $INSTDIR.
Section "-Thalvi install directory"
  ${If} ${Silent}
    Call ThalviDefaultInstallDir
  ${EndIf}
SectionEnd

; Uninstalling also removes the login entry the app's Settings switch wrote; personal config and models stay.
!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Thalvi"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "Thalvi"
!macroend
