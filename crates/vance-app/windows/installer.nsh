; Vance NSIS installer hooks (Tauri v2 → bundle.windows.nsis.installerHooks).
;
; Realises the "installer asks whether to add the optional AI tools" flow on
; Windows. The install itself is delegated to the SAME code path the in-app
; "AI tools" panel uses — `vance-app --install-ai-tools all` — so there is one
; installer, one primitive, one behaviour.
;
; The NSIS bundle is configured `installMode: "currentUser"` (see
; tauri.bundle.conf.json), so this postinstall step runs in the user's own
; context — the managed tools dir it writes to (…\Vance\tools) is per-user,
; which would be WRONG if this ran elevated/perMachine.
;
; If the user declines here, nothing is lost: the app's AI tools panel (and the
; first-run onboarding step) let them add the tools at any time.

!macro NSIS_HOOK_POSTINSTALL
  ; The pip-based tools need a system Python 3. Detect it so we only offer the
  ; heavy install when it can actually succeed; otherwise point at Settings.
  nsExec::ExecToStack 'cmd /c where python || where python3'
  Pop $0 ; return code (0 = found)

  ${If} $0 == 0
    MessageBox MB_YESNO|MB_ICONQUESTION \
      "Add Vance's optional AI tools now?$\r$\n$\r$\n\
       Speech-to-text captions, voice isolation, subject masking, and \
       generative images. This downloads several hundred MB and can take a few \
       minutes.$\r$\n$\r$\n\
       You can skip this and add them any time from the AI tools panel." \
      IDNO vance_skip_ai
    ; Fire-and-forget: launch the install so the installer can finish. Progress
    ; is visible later in the app's AI tools panel; exit code isn't awaited.
    Exec '"$INSTDIR\vance-app.exe" --install-ai-tools all'
    Goto vance_skip_ai
  ${Else}
    MessageBox MB_OK|MB_ICONINFORMATION \
      "Vance installed.$\r$\n$\r$\n\
       The optional AI tools need Python 3 (not detected). Install Python 3, \
       then add them from the AI tools panel inside Vance."
  ${EndIf}

  vance_skip_ai:
!macroend
