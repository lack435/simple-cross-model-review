<#
.SYNOPSIS
  End-to-end check: drive cross-review.exe over stdio exactly as an MCP client would.

.DESCRIPTION
  Speaks real MCP to the server on stdin/stdout and performs a full round trip:
  initialize, tools/list, a live review, and a resumed follow-up review. This calls
  the configured reviewer model for real, so it costs tokens and takes a few minutes.

  Unit tests cover parsing and failure classification; this covers the parts only a
  real CLI can prove: that the reviewer runs, that a review comes back, and that a
  session resumes.

.EXAMPLE
  .\smoke.ps1 -Reviewer codex
  .\smoke.ps1 -Reviewer claude -Effort low
#>
[CmdletBinding()]
param(
    [ValidateSet('codex', 'claude')]
    [string]$Reviewer = 'codex',

    [string]$Model,

    # Low effort keeps the smoke test cheap; the pinned defaults are for real reviews.
    [string]$Effort = 'low',

    # Path to the reviewer CLI, when it is not on PATH.
    [string]$ReviewerBin,

    [string]$Exe = (Join-Path $PSScriptRoot 'target\release\cross-review.exe')
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Exe)) {
    throw "cross-review.exe not found at $Exe. Run .\build.ps1 first."
}

$serverArgs = @('--reviewer', $Reviewer, '--effort', $Effort, '--timeout-seconds', '600')
if ($Model) { $serverArgs += @('--model', $Model) }
if ($ReviewerBin) { $serverArgs += @('--bin', $ReviewerBin) }

# Keep smoke-test sessions out of the real per-project state.
$stateDir = Join-Path ([System.IO.Path]::GetTempPath()) "cross-review-smoke-$PID"
$serverArgs += @('--state-dir', $stateDir)

Write-Host "==> launching: $Exe $($serverArgs -join ' ')" -ForegroundColor Cyan

# Windows PowerShell 5.1 runs on .NET Framework, whose ProcessStartInfo has no
# ArgumentList, so the command line is assembled and quoted by hand.
function Format-Argument {
    param([string]$Value)
    if ($Value -match '[\s"]') { return '"' + ($Value -replace '"', '\"') + '"' }
    return $Value
}

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Exe
$psi.Arguments = ($serverArgs | ForEach-Object { Format-Argument $_ }) -join ' '
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.WorkingDirectory = $PSScriptRoot

$proc = [System.Diagnostics.Process]::Start($psi)

# PowerShell's default StandardInput writer emits a UTF-8 BOM on the first write, which
# is not valid JSON. Write BOM-less UTF-8 instead.
$stdin = New-Object System.IO.StreamWriter($proc.StandardInput.BaseStream, (New-Object System.Text.UTF8Encoding($false)))
$stdin.AutoFlush = $true

# Collect the server's stderr so diagnostics are visible if something goes wrong.
$stderrBuffer = New-Object System.Text.StringBuilder
$stderrHandler = {
    if ($EventArgs.Data) { [void]$Event.MessageData.AppendLine($EventArgs.Data) }
}
$stderrSub = Register-ObjectEvent -InputObject $proc -EventName ErrorDataReceived `
    -Action $stderrHandler -MessageData $stderrBuffer
$proc.BeginErrorReadLine()

$script:nextId = 0
$failures = New-Object System.Collections.Generic.List[string]

function Send-Rpc {
    param(
        [Parameter(Mandatory)][string]$Method,
        $Params,
        [switch]$Notification,
        [int]$TimeoutSeconds = 420
    )

    $message = [ordered]@{ jsonrpc = '2.0'; method = $Method }
    if (-not $Notification) {
        $script:nextId++
        $message['id'] = $script:nextId
    }
    if ($null -ne $Params) { $message['params'] = $Params }

    $json = $message | ConvertTo-Json -Depth 12 -Compress
    $stdin.WriteLine($json)
    $stdin.Flush()

    if ($Notification) { return $null }

    # ReadLineAsync so a hung server surfaces as a timeout instead of blocking forever.
    $task = $proc.StandardOutput.ReadLineAsync()
    if (-not $task.Wait([TimeSpan]::FromSeconds($TimeoutSeconds))) {
        throw "timed out after ${TimeoutSeconds}s waiting for a response to '$Method'"
    }
    $line = $task.Result
    if ($null -eq $line) { throw "server closed stdout while handling '$Method'" }
    return $line | ConvertFrom-Json
}

function Assert-That {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][bool]$Condition, [string]$Detail)
    if ($Condition) {
        Write-Host "  PASS  $Name" -ForegroundColor Green
    }
    else {
        Write-Host "  FAIL  $Name" -ForegroundColor Red
        if ($Detail) { Write-Host "        $Detail" -ForegroundColor DarkYellow }
        $failures.Add($Name)
    }
}

function Get-ToolText {
    param($Response)
    if ($null -eq $Response.result) { return '' }
    return ($Response.result.content | ForEach-Object { $_.text }) -join "`n"
}

try {
    Write-Host "`n=== 1. initialize ===" -ForegroundColor Cyan
    $init = Send-Rpc -Method 'initialize' -Params @{
        protocolVersion = '2025-06-18'
        capabilities    = @{}
        clientInfo      = @{ name = 'smoke.ps1'; version = '1.0' }
    } -TimeoutSeconds 30
    Assert-That 'server identifies itself' ($init.result.serverInfo.name -eq 'cross-review') `
        "got: $($init.result.serverInfo.name)"
    Assert-That 'protocol version is negotiated' ($init.result.protocolVersion -eq '2025-06-18') `
        "got: $($init.result.protocolVersion)"
    Assert-That 'tools capability is advertised' ($null -ne $init.result.capabilities.tools)

    Send-Rpc -Method 'notifications/initialized' -Notification | Out-Null

    Write-Host "`n=== 2. tools/list ===" -ForegroundColor Cyan
    $list = Send-Rpc -Method 'tools/list' -TimeoutSeconds 30
    $names = @($list.result.tools | ForEach-Object { $_.name })
    Assert-That 'four tools are exposed' ($names.Count -eq 4) "got: $($names -join ', ')"
    Assert-That 'cross_model_review is present' ($names -contains 'cross_model_review')
    Assert-That 'cross_model_review_result is present' ($names -contains 'cross_model_review_result')

    Write-Host "`n=== 3. status (reviewer CLI and auth) ===" -ForegroundColor Cyan
    $status = Send-Rpc -Method 'tools/call' -Params @{
        name = 'cross_model_review_status'; arguments = @{}
    } -TimeoutSeconds 90
    $statusText = Get-ToolText $status
    Write-Host $statusText
    Assert-That 'status reports the tool is ready' ($statusText -match 'ready:\s+yes') `
        'the reviewer CLI is missing or not signed in; the rest of the smoke test cannot run'

    if ($statusText -notmatch 'ready:\s+yes') {
        throw 'reviewer not ready'
    }

    Write-Host "`n=== 4. a real review ===" -ForegroundColor Cyan
    $start = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review'
        arguments = @{
            instructions  = @'
This is an automated smoke test of a review tool. Do not review any code.
Reply with exactly two lines and nothing else:
SMOKE-OK
COUNTER=1
'@
            session       = 'smoke'
        }
    } -TimeoutSeconds 60
    $startText = Get-ToolText $start
    Assert-That 'review start is not an error' ($start.result.isError -eq $false) $startText
    $reviewId = ([regex]::Match($startText, 'review_id:\s*(\S+)')).Groups[1].Value
    Assert-That 'a review_id was returned' (-not [string]::IsNullOrWhiteSpace($reviewId)) $startText
    Write-Host "  review_id: $reviewId"

    Write-Host '  waiting for the reviewer...' -ForegroundColor DarkGray
    $collected = $null
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        $collected = Send-Rpc -Method 'tools/call' -Params @{
            name      = 'cross_model_review_result'
            arguments = @{ review_id = $reviewId; wait_seconds = 120 }
        } -TimeoutSeconds 300
        $text = Get-ToolText $collected
        if ($text -notmatch 'status:\s+running') { break }
        Write-Host "  still running (poll $attempt)" -ForegroundColor DarkGray
    }
    $resultText = Get-ToolText $collected
    Assert-That 'review completed without error' ($collected.result.isError -eq $false) $resultText
    Assert-That 'review reports completed status' ($resultText -match 'status:\s+completed') $resultText
    Assert-That 'the reviewer actually answered' ($resultText -match 'SMOKE-OK') $resultText
    Assert-That 'review body is delimited' ($resultText -match 'BEGIN REVIEW') $resultText
    Write-Host $resultText

    Write-Host "`n=== 5. resuming the same review session ===" -ForegroundColor Cyan
    $resume = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review'
        arguments = @{
            instructions = @'
Still the smoke test. Increment the counter you reported before.
Reply with exactly two lines and nothing else:
SMOKE-OK
COUNTER=2
'@
            session      = 'smoke'
        }
    } -TimeoutSeconds 60
    $resumeText = Get-ToolText $resume
    Assert-That 'resume start is not an error' ($resume.result.isError -eq $false) $resumeText
    Assert-That 'session was resumed rather than recreated' ($resumeText -match 'resumed, turn 2') $resumeText
    $resumeId = ([regex]::Match($resumeText, 'review_id:\s*(\S+)')).Groups[1].Value

    $collected2 = $null
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        $collected2 = Send-Rpc -Method 'tools/call' -Params @{
            name      = 'cross_model_review_result'
            arguments = @{ review_id = $resumeId; wait_seconds = 120 }
        } -TimeoutSeconds 300
        $text = Get-ToolText $collected2
        if ($text -notmatch 'status:\s+running') { break }
        Write-Host "  still running (poll $attempt)" -ForegroundColor DarkGray
    }
    $resumeResult = Get-ToolText $collected2
    Assert-That 'resumed review completed' ($resumeResult -match 'status:\s+completed') $resumeResult
    Assert-That 'reviewer remembered the earlier turn' ($resumeResult -match 'COUNTER=2') $resumeResult
    Write-Host $resumeResult

    Write-Host "`n=== 6. error handling ===" -ForegroundColor Cyan
    $bad = Send-Rpc -Method 'tools/call' -Params @{
        name = 'cross_model_review'; arguments = @{ session = 'nope' }
    } -TimeoutSeconds 30
    $badText = Get-ToolText $bad
    Assert-That 'missing instructions is rejected' ($bad.result.isError -eq $true) $badText
    Assert-That 'rejection names the offending field' ($badText -match 'instructions') $badText

    $unknown = Send-Rpc -Method 'tools/call' -Params @{
        name = 'no_such_tool'; arguments = @{}
    } -TimeoutSeconds 30
    Assert-That 'unknown tool is an isError result' ($unknown.result.isError -eq $true)

    $badMethod = Send-Rpc -Method 'not/a/method' -TimeoutSeconds 30
    Assert-That 'unknown method is a JSON-RPC error' ($badMethod.error.code -eq -32601)

    Write-Host "`n=== 7. sessions persist on disk ===" -ForegroundColor Cyan
    $sessionsFile = Join-Path $stateDir 'sessions.json'
    Assert-That 'session state was written' (Test-Path $sessionsFile) $sessionsFile
    if (Test-Path $sessionsFile) {
        $saved = Get-Content $sessionsFile -Raw | ConvertFrom-Json
        Assert-That 'smoke session recorded two turns' ($saved.sessions.smoke.turns -eq 2) `
            (Get-Content $sessionsFile -Raw)
    }
}
finally {
    if ($proc -and -not $proc.HasExited) {
        $stdin.Close()
        if (-not $proc.WaitForExit(5000)) { $proc.Kill() }
    }
    if ($stderrSub) { Unregister-Event -SubscriptionId $stderrSub.Id -ErrorAction SilentlyContinue }

    $stderrText = $stderrBuffer.ToString()
    if ($stderrText.Trim()) {
        Write-Host "`n--- server stderr ---" -ForegroundColor DarkGray
        Write-Host $stderrText -ForegroundColor DarkGray
    }
    Remove-Item $stateDir -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "SMOKE TEST PASSED (reviewer: $Reviewer)" -ForegroundColor Green
    exit 0
}
Write-Host "SMOKE TEST FAILED: $($failures.Count) check(s)" -ForegroundColor Red
$failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
exit 1
