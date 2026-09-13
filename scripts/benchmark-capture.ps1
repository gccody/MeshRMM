# Run from an interactive Windows desktop. Opens a temporary animated window
# and measures the real Desktop Duplication -> GPU conversion -> HEVC pipeline.
$ErrorActionPreference = 'Stop'
Push-Location (Split-Path $PSScriptRoot -Parent)
try {
    $build = & cargo test -p meshrmm-remote-screen --release --no-run --message-format=json
    if ($LASTEXITCODE -ne 0) { throw 'Benchmark build failed' }
    $executable = $build | ForEach-Object { $_ | ConvertFrom-Json } |
        Where-Object { $_.reason -eq 'compiler-artifact' -and $_.profile.test -and $_.executable } |
        Select-Object -Last 1 -ExpandProperty executable
    if (-not $executable) { throw 'Benchmark executable was not produced' }

    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing
    $form = New-Object System.Windows.Forms.Form
    $form.Text = 'MeshRMM capture benchmark (closes automatically)'
    $form.Width = 1000
    $form.Height = 700
    $form.StartPosition = 'CenterScreen'
    $form.TopMost = $true
    $timer = New-Object System.Windows.Forms.Timer
    $timer.Interval = 1
    $script:tick = 0
    $script:benchmarkProcess = $null
    $stdoutPath = [System.IO.Path]::GetTempFileName()
    $stderrPath = [System.IO.Path]::GetTempFileName()
    $timer.Add_Tick({
        $script:tick++
        $form.BackColor = [System.Drawing.Color]::FromArgb(($script:tick * 3) % 256, 80, 160)
        if ($script:benchmarkProcess -and $script:benchmarkProcess.HasExited) { $form.Close() }
    })
    $form.Add_Shown({
        $script:benchmarkProcess = Start-Process -FilePath $executable -NoNewWindow -PassThru `
            -ArgumentList 'desktop_capture_throughput --ignored --nocapture' `
            -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
        $null = $script:benchmarkProcess.Handle
        $timer.Start()
    })
    try {
        [System.Windows.Forms.Application]::Run($form)
        if ($script:benchmarkProcess) {
            $script:benchmarkProcess.WaitForExit()
            Get-Content $stdoutPath
            Get-Content $stderrPath
            if ($script:benchmarkProcess.ExitCode -ne 0) { throw 'Capture benchmark failed' }
        }
    } finally {
        Remove-Item $stdoutPath, $stderrPath -ErrorAction SilentlyContinue
        $timer.Dispose()
        $form.Dispose()
        if ($script:benchmarkProcess) { $script:benchmarkProcess.Dispose() }
    }
} finally {
    Pop-Location
}
