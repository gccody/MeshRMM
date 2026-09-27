# Dot-source before cargo so the opus crate can build its bundled libopus.
# Keeps an existing CMAKE or a cmake on PATH; otherwise uses the CMake that
# ships with Visual Studio ("C++ CMake tools for Windows").
if ($env:CMAKE -and (Test-Path -LiteralPath $env:CMAKE)) {
    return
}
if (Get-Command cmake -ErrorAction SilentlyContinue) {
    return
}
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path -LiteralPath $vswhere) {
    $installations = @(& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.CMake.Project -property installationPath)
    if ($LASTEXITCODE -eq 0) {
        foreach ($installation in $installations) {
            $candidate = Join-Path $installation 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe'
            if (Test-Path -LiteralPath $candidate) {
                $env:CMAKE = $candidate
                Write-Output "Using CMake from $candidate"
                return
            }
        }
    }
}
throw 'CMake is required to build libopus. Install CMake, or the Visual Studio "C++ CMake tools for Windows" component.'
