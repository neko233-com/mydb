Option Explicit

Dim fso, shell, scriptPath, commandLine, index, hasQuiet
Set fso = CreateObject("Scripting.FileSystemObject")
Set shell = CreateObject("Shell.Application")

scriptPath = fso.BuildPath(fso.GetParentFolderName(WScript.ScriptFullName), "install.ps1")
If Not fso.FileExists(scriptPath) Then
    WScript.Quit 2
End If

commandLine = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File " & QuoteArg(scriptPath)
hasQuiet = False
For index = 0 To WScript.Arguments.Count - 1
    If LCase(WScript.Arguments(index)) = "-quiet" Then
        hasQuiet = True
    End If
    commandLine = commandLine & " " & QuoteArg(WScript.Arguments(index))
Next
If Not hasQuiet Then
    commandLine = commandLine & " -Quiet"
End If

' The runas verb may show the normal Windows UAC consent prompt; the PowerShell
' process itself is hidden and does not take over the user's foreground window.
shell.ShellExecute "powershell.exe", commandLine, "", "runas", 0

Function QuoteArg(value)
    If InStr(value, " ") > 0 Or InStr(value, vbTab) > 0 Or InStr(value, """") > 0 Then
        QuoteArg = """" & Replace(value, """", """""") & """"
    Else
        QuoteArg = value
    End If
End Function
