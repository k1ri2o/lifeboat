; Installs the Lifeboat background service next to the app. The installer runs as admin
; (perMachine), so the service exe and restic sit in Program Files, where normal programs
; cannot replace them. Uninstalling removes the service only: backups on the hidden
; partition are kept.

!macro NSIS_HOOK_PREINSTALL
  ; Upgrade: stop the running service so its exe can be replaced.
  nsExec::Exec 'sc.exe stop lifeboat-svc'
  Sleep 3000
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Fails harmlessly on upgrade, when the service already exists.
  nsExec::ExecToLog '"$INSTDIR\lifeboat-svc.exe" install'
  nsExec::ExecToLog 'sc.exe start lifeboat-svc'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog '"$INSTDIR\lifeboat-svc.exe" uninstall'
  Sleep 3000
!macroend
