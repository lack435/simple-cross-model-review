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
$script:pendingRead = $null
$script:progressMessages = New-Object System.Collections.Generic.List[object]
$failures = New-Object System.Collections.Generic.List[string]

# One read at a time, and a read that times out stays outstanding rather than being
# abandoned. An abandoned ReadLineAsync still consumes the next line the server writes,
# which would silently shift every later response onto the wrong request -- and the
# cancellation check below deliberately waits for a line that must never arrive.
function Read-Line {
    param([int]$TimeoutSeconds)
    if (-not $script:pendingRead) { $script:pendingRead = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:pendingRead.Wait([TimeSpan]::FromSeconds($TimeoutSeconds))) { return $null }
    $line = $script:pendingRead.Result
    $script:pendingRead = $null
    if ($null -eq $line) { throw 'server closed stdout' }
    return $line
}

function Send-Rpc {
    param(
        [Parameter(Mandatory)][string]$Method,
        $Params,
        [switch]$Notification,
        # Write the request and return its id without reading the response, for the
        # cases that need to interleave another message before collecting it.
        [switch]$NoWait,
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
    if ($NoWait) { return $script:nextId }

    # One deadline for the request. Progress notifications prove liveness but must not
    # reset the harness timeout forever if the server never sends a terminal response.
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ($true) {
        $remaining = [int][Math]::Ceiling(($deadline - [DateTime]::UtcNow).TotalSeconds)
        if ($remaining -le 0) {
            throw "timed out after ${TimeoutSeconds}s waiting for a response to '$Method'"
        }
        $line = Read-Line -TimeoutSeconds $remaining
        if ($null -eq $line) {
            throw "timed out after ${TimeoutSeconds}s waiting for a response to '$Method'"
        }
        $parsed = $line | ConvertFrom-Json
        # Progress is a notification rather than a response, so consume and display it
        # without losing the response still owed to this request.
        if ($parsed.method -eq 'notifications/progress') {
            $script:progressMessages.Add($parsed)
            Write-Host "  progress: $($parsed.params.message)" -ForegroundColor DarkGray
            continue
        }
        # Responses are matched by id, not by position. The cancellation stage deliberately
        # interleaves messages, so a suppressed response that turned out not to be suppressed
        # would otherwise be read as the answer to the next request and quietly mislead.
        if ($parsed.id -ne $message['id']) {
            throw "response id $($parsed.id) does not answer '$Method' (id $($message['id'])): $line"
        }
        return $parsed
    }
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
    $progressBefore = $script:progressMessages.Count
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        $collected = Send-Rpc -Method 'tools/call' -Params @{
            name      = 'cross_model_review_result'
            arguments = @{ review_id = $reviewId; wait_seconds = 120 }
            _meta     = @{ progressToken = "smoke-first-$attempt" }
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
    Assert-That 'the wait emitted MCP progress notifications' `
        ($script:progressMessages.Count -gt $progressBefore)
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
    $resumeProgressBefore = $script:progressMessages.Count
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        $collected2 = Send-Rpc -Method 'tools/call' -Params @{
            name      = 'cross_model_review_result'
            arguments = @{ review_id = $resumeId; wait_seconds = 120 }
            _meta     = @{ progressToken = "smoke-resume-$attempt" }
        } -TimeoutSeconds 300
        $text = Get-ToolText $collected2
        if ($text -notmatch 'status:\s+running') { break }
        Write-Host "  still running (poll $attempt)" -ForegroundColor DarkGray
    }
    $resumeResult = Get-ToolText $collected2
    Assert-That 'resumed review completed' ($resumeResult -match 'status:\s+completed') $resumeResult
    Assert-That 'reviewer remembered the earlier turn' ($resumeResult -match 'COUNTER=2') $resumeResult
    Assert-That 'the resumed wait emitted MCP progress notifications' `
        ($script:progressMessages.Count -gt $resumeProgressBefore)
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

    # Cancellation has two contracts now, and they differ. Abandoning a cross_model_review_result
    # poll must leave the reviewer RUNNING (only the wait detaches); cross_model_review_cancel must
    # STOP it. These e2e checks are best-effort against real-model timing -- the deterministic proof
    # lives in the unit tests -- so they tolerate a review that finished unusually fast. Separate
    # sessions, so cancelling them cannot disturb the turn count checked below.
    Write-Host "`n=== 7a. a cancelled result poll leaves the review running ===" -ForegroundColor Cyan
    $doomed = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review'
        arguments = @{
            instructions = 'Smoke test of poll cancellation. Wait quietly; the poll will be cancelled.'
            session      = 'smoke-cancel'
            fresh        = $true
        }
    } -TimeoutSeconds 60
    $doomedText = Get-ToolText $doomed
    Assert-That 'a cancellable review started' ($doomed.result.isError -eq $false) $doomedText
    $doomedId = ([regex]::Match($doomedText, 'review_id:\s*(\S+)')).Groups[1].Value

    # Poll for it, then abandon the poll the way a real client does. wait_seconds is kept
    # short on purpose: with a long wait, "no line arrived" would prove nothing, because a
    # server that ignored the cancellation entirely would also still be waiting. At 20s
    # the poll is certain to have returned by the time the window below closes, so silence
    # can only mean the response was suppressed.
    $pollId = Send-Rpc -Method 'tools/call' -NoWait -Params @{
        name      = 'cross_model_review_result'
        arguments = @{ review_id = $doomedId; wait_seconds = 20 }
    }
    Start-Sleep -Seconds 2
    Send-Rpc -Method 'notifications/cancelled' -Notification -Params @{
        requestId = $pollId; reason = 'smoke test'
    } | Out-Null

    # Expected: no response, because the cancellation suppressed it. Tolerated: a trivial review
    # under --effort low can finish inside the 2s window, in which case the poll's own completed
    # response was legitimately already queued and a line arrives. Either is fine here -- response
    # suppression is proved deterministically by the unit tests; what this e2e run checks is that
    # the review was not destroyed, which the collect below establishes in both cases.
    $stray = Read-Line -TimeoutSeconds 40
    if ($null -ne $stray) {
        Write-Host "  (note: a poll response arrived before the cancellation could land; tolerated)" `
            -ForegroundColor Yellow
    }

    # The reviewer itself must still be alive and collectible: a poll cancellation detaches, it
    # does not kill. A fresh collect returns running (or completed if it finished), never CANCELLED.
    $after = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review_result'
        arguments = @{ review_id = $doomedId; wait_seconds = 5 }
    } -TimeoutSeconds 60
    $afterText = Get-ToolText $after
    Assert-That 'the poll cancellation left the review running or collectible' `
        (($afterText -match 'status:\s+running') -or ($afterText -match 'status:\s+completed')) $afterText
    Assert-That 'the poll cancellation did not cancel the review' ($afterText -notmatch 'CANCELLED') $afterText

    # Clean up so this review does not keep billing through the rest of the run.
    Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review_cancel'
        arguments = @{ review_id = $doomedId }
    } -TimeoutSeconds 30 | Out-Null

    Write-Host "`n=== 7b. cross_model_review_cancel stops the reviewer ===" -ForegroundColor Cyan
    $doomedB = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review'
        arguments = @{
            instructions = 'Smoke test of explicit cancellation. Wait quietly; this will be cancelled.'
            session      = 'smoke-cancel-b'
            fresh        = $true
        }
    } -TimeoutSeconds 60
    $doomedBText = Get-ToolText $doomedB
    Assert-That 'a second cancellable review started' ($doomedB.result.isError -eq $false) $doomedBText
    $doomedBId = ([regex]::Match($doomedBText, 'review_id:\s*(\S+)')).Groups[1].Value

    Start-Sleep -Seconds 2
    $cancelled = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review_cancel'
        arguments = @{ review_id = $doomedBId }
    } -TimeoutSeconds 30
    $cancelledText = Get-ToolText $cancelled
    # Tolerant of a review that finished before the cancel landed: either it was stopped, or it had
    # already finished. Both prove the cancel reached a real review.
    Assert-That 'the explicit cancel stopped the reviewer or it had already finished' `
        (($cancelledText -match 'was cancelled') -or ($cancelledText -match 'had already finished')) $cancelledText

    # And a subsequent collect must confirm it is not still running.
    $afterB = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review_result'
        arguments = @{ review_id = $doomedBId; wait_seconds = 15 }
    } -TimeoutSeconds 60
    $afterBText = Get-ToolText $afterB
    Assert-That 'the cancelled review is not still running' ($afterBText -notmatch 'status:\s+running') $afterBText

    $ping = Send-Rpc -Method 'ping' -TimeoutSeconds 30
    Assert-That 'the server is still healthy afterwards' ($null -ne $ping.result) `
        'the server stopped answering after handling a cancellation'

    Write-Host "`n=== 8. sessions persist on disk ===" -ForegroundColor Cyan
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
