<#
.SYNOPSIS
Temporarily pause one installed Agent helper to test service isolation.
.DESCRIPTION
Run as administrator on the Windows test endpoint. Select a helper
PID from the Agent startup log. The script resumes the process in finally, including when sleep is interrupted.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][int]$HelperProcessId,
    [ValidateRange(1,120)][int]$Seconds = 30
)
$ErrorActionPreference = 'Stop'
$helper = Get-CimInstance Win32_Process -Filter "ProcessId=$HelperProcessId"
if ($helper.Name -ne 'meshrmm-agent.exe' -or $helper.CommandLine -notmatch ' --capture-helper$') {
    throw 'The selected process is not a MeshRMM desktop helper.'
}
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class HelperPause {
    [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr OpenProcess(uint access, bool inherit, int id);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr handle);
    [DllImport("ntdll.dll")] public static extern int NtSuspendProcess(IntPtr handle);
    [DllImport("ntdll.dll")] public static extern int NtResumeProcess(IntPtr handle);
}
'@
$handle = [HelperPause]::OpenProcess(0x0800, $false, $HelperProcessId)
if ($handle -eq [IntPtr]::Zero) { throw "OpenProcess failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }
$paused = $false
try {
    $status = [HelperPause]::NtSuspendProcess($handle)
    if ($status -ne 0) { throw "NtSuspendProcess failed: $status" }
    $paused = $true
    Write-Output "SUSPENDED helper=$HelperProcessId at=$([DateTime]::UtcNow.ToString('o')) duration_seconds=$Seconds"
    Start-Sleep -Seconds $Seconds
} finally {
    if ($paused) {
        $status = [HelperPause]::NtResumeProcess($handle)
        Write-Output "RESUMED helper=$HelperProcessId at=$([DateTime]::UtcNow.ToString('o')) status=$status"
    }
    [void][HelperPause]::CloseHandle($handle)
}
