Option Explicit
Dim shell, files, script, command, result, resultRoot, resultPath, detail, reader
Set shell = CreateObject("WScript.Shell")
Set files = CreateObject("Scripting.FileSystemObject")
script = files.BuildPath(files.GetParentFolderName(WScript.ScriptFullName), "update-local-clipferry.ps1")
command = """" & shell.ExpandEnvironmentStrings("%SystemRoot%") & "\System32\WindowsPowerShell\v1.0\powershell.exe"" -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File """ & script & """"
If WScript.Arguments.Named.Exists("check") Then
    WScript.Echo command
    WScript.Quit 0
End If
resultRoot = shell.ExpandEnvironmentStrings("%LOCALAPPDATA%") & "\ClipFerry\update-results"
If Not files.FolderExists(resultRoot) Then files.CreateFolder resultRoot
resultPath = files.BuildPath(resultRoot, files.GetTempName)
command = command & " -ResultPath """ & resultPath & """"
On Error Resume Next
result = shell.Run(command, 0, True)
If Err.Number <> 0 Then
    MsgBox "Cannot start Pierdesk updater: " & Err.Description, 16, "Pierdesk"
    WScript.Quit 1
End If
On Error GoTo 0
If result <> 0 Then
    detail = "No detailed result was returned. Logs: " & shell.ExpandEnvironmentStrings("%LOCALAPPDATA%") & "\ClipFerry\updates"
    If files.FileExists(resultPath) Then
        Set reader = files.OpenTextFile(resultPath, 1, False, -1)
        detail = reader.ReadAll
        reader.Close
    End If
    MsgBox "Pierdesk update did not complete:" & vbCrLf & vbCrLf & detail, 16, "Pierdesk"
End If
WScript.Quit result
