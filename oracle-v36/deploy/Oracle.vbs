' Oracle of Delphi — one-click, windowless launcher.
'
' Double-click this to bring the WHOLE assistant up with no PowerShell windows:
' it starts oracle-core, which in turn launches the LLM server and the actd
' daemon as hidden background processes and opens the Oracle's face in a
' chromeless window. Running it again just summons the existing instance (core
' notices it's already up and only opens the window).
'
' Put a shortcut to this in shell:startup to have the Oracle available at login.
Option Explicit
Dim shell, fso, here, bat
Set shell = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")

here = fso.GetParentFolderName(WScript.ScriptFullName)
bat = fso.BuildPath(here, "oracle-run.bat")

' Window style 0 = hidden. False = don't block. The .bat does the real work and
' captures core's log; core spawns its children with no console windows.
shell.Run """" & bat & """", 0, False
