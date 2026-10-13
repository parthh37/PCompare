; Explorer context menu, like Beyond Compare's shell extension:
;   "Select Left Side for Compare", then "Compare to '<name>'" on another item,
;   or select 2-3 items -> "Compare with PCompare".
; The app writes the verbs itself (per user, HKCU) from the list in
; crates/pcompare-shell/src/menu.rs, the same code it runs at startup and from
; Settings, so the installer, MSI and portable copies all get the same menu.

!macro NSIS_HOOK_POSTINSTALL
  ; Respects "off" in Settings.
  ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --install-shell-menu'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; An update reinstalls straight after; only a real uninstall removes the menu.
  ${If} $UpdateMode <> 1
    ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-shell-menu'
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Verbs written by 0.2.0's installer, in case the exe above couldn't run.
  ${If} $UpdateMode <> 1
    DeleteRegKey HKCU "Software\Classes\*\shell\PCompare.SelectLeft"
    DeleteRegKey HKCU "Software\Classes\*\shell\PCompare.CompareTo"
    DeleteRegKey HKCU "Software\Classes\Directory\shell\PCompare.SelectLeft"
    DeleteRegKey HKCU "Software\Classes\Directory\shell\PCompare.CompareTo"
    System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
  ${EndIf}
!macroend
