# Run from an interactive Windows user account with Power BI Desktop installed.
# Uses only a newly created synthetic project, never an existing report.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Executable,
    [Parameter(Mandatory)][string]$Probe,
    [Parameter(Mandatory)][string]$OutputRoot,
    [string]$Desktop = "$env:ProgramFiles\Microsoft Power BI Desktop\bin\PBIDesktop.exe"
)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path $Executable).Path
$probeDll = (Resolve-Path $Probe).Path
if (-not (Test-Path $Desktop -PathType Leaf)) { throw "Desktop executable missing: $Desktop" }
if (Test-Path $OutputRoot) { throw "Choose a fresh output root: $OutputRoot" }
$work = (New-Item -ItemType Directory -Path $OutputRoot).FullName
$diagnostics = New-Item -ItemType Directory (Join-Path $work diagnostics)
$project = Join-Path $work PriceSmoke
$entry = Join-Path $project PriceSmoke.pbip
$fixture = Join-Path $work sales.csv
$shell = New-Object -ComObject WScript.Shell
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$rows = @('Make,Selling_Price,Model_Year')
for ($i=0; $i -lt 16; $i++) {
    $make = @('Ford', 'Toyota')[$i % 2]
    $rows += "$make,$(100 + 10 * $i),$(2000 + $i)"
}
[IO.File]::WriteAllLines($fixture, $rows)
& $exe powerbi export $fixture -o $project
if ($LASTEXITCODE -ne 0) { throw "DataDuck export exited $LASTEXITCODE" }
$currentUser = [Security.Principal.WindowsIdentity]::GetCurrent()
Write-Host "Desktop $((Get-Item $Desktop).VersionInfo.ProductVersion), session $([Diagnostics.Process]::GetCurrentProcess().SessionId), interactive $([Environment]::UserInteractive), system account $($currentUser.IsSystem)"
if ($currentUser.IsSystem) { throw 'Power BI Desktop with WebView2 is unsupported under the Windows SYSTEM account.' }

function Write-Diagnostics($process, $stage) {
    Get-Process -Name PBIDesktop,msmdsrv -ErrorAction SilentlyContinue |
        Select-Object Id,ProcessName,SessionId,MainWindowTitle,Responding |
        Out-File (Join-Path $diagnostics "$stage-processes.txt")
    if ($null -eq $process -or $process.HasExited) { return }
    try {
        $cond = New-Object Windows.Automation.PropertyCondition ([Windows.Automation.AutomationElement]::ProcessIdProperty,$process.Id)
        $windows = [Windows.Automation.AutomationElement]::RootElement.FindAll([Windows.Automation.TreeScope]::Children,$cond)
        foreach ($win in $windows) {
            "WINDOW $($win.Current.Name)" | Out-File (Join-Path $diagnostics "$stage-ui.txt") -Append
            $nodes = $win.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)
            $nodes | Select-Object -First 160 | ForEach-Object {
                "$($_.Current.ControlType.ProgrammaticName): $($_.Current.Name)"
            } | Out-File (Join-Path $diagnostics "$stage-ui.txt") -Append
        }
    } catch { Write-Host "UI diagnostics unavailable: $_" }
}

function Start-Report {
    return Start-Process $Desktop -ArgumentList ('"{0}"' -f $entry) -PassThru
}

function Get-ReportPort($process) {
    $until = (Get-Date).AddMinutes(3)
    while ((Get-Date) -lt $until) {
        if ($process.HasExited) { throw "Desktop exited during open: $($process.ExitCode)" }
        $engines = @(Get-CimInstance Win32_Process -Filter "Name='msmdsrv.exe' AND ParentProcessId=$($process.Id)")
        foreach ($engine in $engines) {
            $ports = @(Get-NetTCPConnection -OwningProcess $engine.ProcessId -State Listen -ErrorAction SilentlyContinue |
                Where-Object { $_.LocalAddress -in @('127.0.0.1', '::1') } | Select-Object -ExpandProperty LocalPort -Unique)
            if ($ports.Count -eq 1) { return $ports[0] }
        }
        Start-Sleep -Seconds 2
    }
    throw "Timed out waiting for the process-local Analysis Services listener for Desktop PID $($process.Id)"
}

function Verify-Model($port,$operation) {
    # The engine can listen before the Desktop client has opened its database.
    $until = (Get-Date).AddMinutes(2)
    $probeLog = Join-Path $diagnostics "probe-$operation.log"
    while ((Get-Date) -lt $until) {
        $out = & dotnet $probeDll $port $operation 2>&1
        $code = $LASTEXITCODE
        $out | Out-File $probeLog
        if ($code -eq 0) { $out | Write-Host; return }
        Start-Sleep -Seconds 3
    }
    throw ("The live model probe failed: " + (Get-Content -Raw $probeLog))
}

$desktopProcess = $null
try {
    $desktopProcess = Start-Report
    $port = Get-ReportPort $desktopProcess
    Verify-Model $port refresh
    Write-Diagnostics $desktopProcess loaded

    # The PBIP lives entirely in this disposable directory. Save in the actual Desktop UI.
    if (-not $shell.AppActivate($desktopProcess.Id)) { throw 'Desktop has no foreground window on this runner.' }
    Start-Sleep -Milliseconds 400
    $shell.SendKeys('^s')
    $until = (Get-Date).AddMinutes(1)
    $cache = Join-Path $project 'PriceSmoke.SemanticModel\.pbi\cache.abf'
    while (-not (Test-Path $cache) -and (Get-Date) -lt $until) { Start-Sleep -Seconds 1 }
    if (-not (Test-Path $cache)) { throw 'Save did not create a Desktop import cache in the synthetic project.' }
    if (-not $desktopProcess.CloseMainWindow() -or -not $desktopProcess.WaitForExit(60000)) { throw 'Desktop did not close after saving.' }

    $desktopProcess = Start-Report
    $port = Get-ReportPort $desktopProcess
    Verify-Model $port verify
    Write-Host 'The refreshed project saved, reopened and returned the same values from the persisted model.'
} finally {
    Write-Diagnostics $desktopProcess final
    if ($null -ne $desktopProcess -and -not $desktopProcess.HasExited) {
        Stop-Process -Id $desktopProcess.Id -Force
    }
}
