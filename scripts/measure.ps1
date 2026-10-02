param([string]$Executable = 'target\release\maki-player.exe')
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$exe = (Resolve-Path -LiteralPath (Join-Path $projectRoot $Executable)).Path
$fixtures = Join-Path $projectRoot 'test-output'
if (!(Test-Path -LiteralPath "$fixtures\preview.wav")) { throw 'Run scripts/make-fixtures.py first.' }
$arguments = @('--seconds', '12', '--config-dir', ('"' + "$fixtures\measure-config" + '"'), ('"' + "$fixtures\preview.wav" + '"'))
$clock = [System.Diagnostics.Stopwatch]::StartNew()
$player = Start-Process -FilePath $exe -ArgumentList $arguments -PassThru -WindowStyle Hidden
while (!$player.HasExited -and $player.MainWindowHandle -eq 0 -and $clock.ElapsedMilliseconds -lt 5000) {
    Start-Sleep -Milliseconds 10
    $player.Refresh()
}
$windowMilliseconds = $clock.ElapsedMilliseconds
Start-Sleep -Seconds 3
$player.Refresh()
$cpuBefore = $player.TotalProcessorTime.TotalMilliseconds
$measurement = [System.Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Seconds 5
$player.Refresh()
$cpuPercent = ($player.TotalProcessorTime.TotalMilliseconds - $cpuBefore) / $measurement.Elapsed.TotalMilliseconds * 100
$result = [ordered]@{
    binary_bytes = (Get-Item -LiteralPath $exe).Length
    launch_to_window_handle_ms = $windowMilliseconds
    working_set_bytes = $player.WorkingSet64
    private_bytes = $player.PrivateMemorySize64
    cpu_percent_one_core = [math]::Round($cpuPercent, 3)
    cpu_percent_whole_machine = [math]::Round($cpuPercent / [Environment]::ProcessorCount, 3)
    note = 'One local sample; window handle timing is a startup proxy, not a cold-cache first-paint measurement.'
}
$result | ConvertTo-Json | Set-Content -LiteralPath "$fixtures\performance.json"
$result | ConvertTo-Json
$player.WaitForExit()
if ($player.ExitCode -ne 0) { throw "Player exited with $($player.ExitCode)" }
