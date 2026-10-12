; Explorer context menu, like Beyond Compare's shell extension:
;   1. right-click a file/folder -> "Select Left Side for Compare"
;   2. right-click another one   -> "Compare to Left Side"
; The app records the first path (`--select-left`) and pairs it with the
; second (`--compare-to`). Per-user keys, matching the currentUser install.

!macro PCOMPARE_MENU ROOT
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\PCompare.SelectLeft" "" "Select Left Side for Compare"
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\PCompare.SelectLeft" "Icon" "$INSTDIR\${MAINBINARYNAME}.exe"
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\PCompare.SelectLeft\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" --select-left "%1"'
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\PCompare.CompareTo" "" "Compare to Left Side"
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\PCompare.CompareTo" "Icon" "$INSTDIR\${MAINBINARYNAME}.exe"
  WriteRegStr HKCU "Software\Classes\${ROOT}\shell\PCompare.CompareTo\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" --compare-to "%1"'
!macroend

!macro PCOMPARE_UNMENU ROOT
  DeleteRegKey HKCU "Software\Classes\${ROOT}\shell\PCompare.SelectLeft"
  DeleteRegKey HKCU "Software\Classes\${ROOT}\shell\PCompare.CompareTo"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro PCOMPARE_MENU "*"
  !insertmacro PCOMPARE_MENU "Directory"
  ; Tell Explorer the verbs changed so the menu shows without a restart.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro PCOMPARE_UNMENU "*"
  !insertmacro PCOMPARE_UNMENU "Directory"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
!macroend
