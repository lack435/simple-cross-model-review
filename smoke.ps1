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
  .\smoke.ps1 -Reviewer codex -ProveBlockRepair
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

    # Profile label for the Claude evidence path (see review f2). Must be authorized on this machine.
    [string]$ClaudeProfile = 'work',

    [string]$Exe = (Join-Path $PSScriptRoot 'target\release\cross-review.exe'),

    # Prove the block-repair path against a real reviewer (issue #63). Builds an instrumented
    # binary -- with the non-default `repair-test-hook` feature, which discards each turn's machine
    # block so the repair has to recover it -- into a scratch directory, and runs the round trip
    # through that. Never touches dist\ or any binary built without the feature: the hook is
    # compiled out of everything else, which is the point of it being a Cargo feature rather than
    # an environment variable a shipped binary could inherit.
    [switch]$ProveBlockRepair
)

$ErrorActionPreference = 'Stop'

if ($ProveBlockRepair) {
    $hookTarget = Join-Path ([System.IO.Path]::GetTempPath()) "cross-review-repair-hook-$PID"
    Write-Host "==> building an instrumented binary (repair-test-hook) into $hookTarget" -ForegroundColor Cyan
    Write-Host "    It deliberately discards each turn's machine block, so the block repair has to" -ForegroundColor Yellow
    Write-Host "    recover it. Built outside dist\ and never for a real review." -ForegroundColor Yellow
    # cargo writes its progress to stderr, and Windows PowerShell turns a native command's stderr
    # into ErrorRecords -- which under the script-wide 'Stop' preference kills the build on the
    # first "Compiling ..." line. Drop to 'Continue' for the call and judge it by its exit code,
    # which is the only reliable signal from a native executable here.
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & cargo build --release --features repair-test-hook --target-dir $hookTarget
    $buildExit = $LASTEXITCODE
    $ErrorActionPreference = $prevEap
    if ($buildExit -ne 0) { throw "instrumented build failed (cargo exit $buildExit)" }
    $Exe = Join-Path $hookTarget 'release\cross-review.exe'
}

if (-not (Test-Path $Exe)) {
    throw "cross-review.exe not found at $Exe. Run .\build.ps1 first."
}

$serverArgs = @('--reviewer', $Reviewer, '--effort', $Effort, '--timeout-seconds', '600')
if ($Model) { $serverArgs += @('--model', $Model) }
if ($ReviewerBin) { $serverArgs += @('--bin', $ReviewerBin) }
# The Claude evidence path requires a pinned profile home (an ambient Claude keeps --safe-mode and
# gets no evidence, review f2), so the Claude smoke pins the dogfood "work" profile to exercise it.
# This assumes the profile is authorized on this machine (cross_model_setup_profile). Override with
# -ClaudeProfile to use a different label.
if ($Reviewer -eq 'claude') { $serverArgs += @('--claude-profile', $ClaudeProfile) }

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
    # The exact set, not a count. This asserted "four tools are exposed" and went stale the moment a
    # fifth was added (cross_model_setup_profile), failing with a number rather than a name. Comparing
    # the sorted set says which tool appeared or vanished, and still catches an accidental extra one.
    $expectedTools = @(
        'cross_model_review',
        'cross_model_consult',
        'cross_model_consult_result',
        'cross_model_review_cancel',
        'cross_model_review_result',
        'cross_model_review_status',
        'cross_model_setup_profile'
    )
    $sorted = ($names | Sort-Object) -join ', '
    Assert-That 'exactly the expected tools are exposed' ($sorted -eq (($expectedTools | Sort-Object) -join ', ')) `
        "expected: $(($expectedTools | Sort-Object) -join ', ')`n        got:      $sorted"
    Assert-That 'cross_model_review is present' ($names -contains 'cross_model_review')
    Assert-That 'cross_model_review_result is present' ($names -contains 'cross_model_review_result')
    Assert-That 'cross_model_consult is present' ($names -contains 'cross_model_consult')
    Assert-That 'cross_model_consult_result is present' ($names -contains 'cross_model_consult_result')

    Write-Host "`n=== 3. status (reviewer CLI and auth) ===" -ForegroundColor Cyan
    $status = Send-Rpc -Method 'tools/call' -Params @{
        name = 'cross_model_review_status'; arguments = @{}
    } -TimeoutSeconds 90
    $statusText = Get-ToolText $status
    Write-Host $statusText
    Assert-That 'status reports the tool is ready' ($statusText -match 'ready:\s+yes') `
        'the reviewer CLI is missing or not signed in; the rest of the smoke test cannot run'
    if ($Reviewer -eq 'codex') {
        Assert-That 'Codex evidence handshake is ready' `
            ($statusText -match 'evidence:\s+ready \(schema 2, 7 read-only tools; no-model handshake passed;') `
            $statusText
        # A tree too large to scan no longer refuses the review (issue #86), so "ready" alone no
        # longer says drift is being tracked. This checkout is small, so it must be.
        Assert-That 'drift tracking is reported and available here' `
            ($statusText -match 'drift tracking: on \(') `
            $statusText
    }

    if ($statusText -notmatch 'ready:\s+yes') {
        throw 'reviewer not ready'
    }

    Write-Host "`n=== 4. a real review ===" -ForegroundColor Cyan
    # Both reviewers now have the read-only evidence service, so both are asked to exercise it. For
    # Claude this also satisfies the section-7 gate on the smoke's empty capture: the gate requires a
    # successful content evidence call before an empty-capture review is trusted.
    $firstInstructions = @'
This is an automated smoke test of a review tool. Do not review any code. Do not use the shell.
Call repository_scope, repository_list for the repository root, repository_search for the literal
"cross-review", and repository_diff with base "branch-base" and head "worktree" (page it to the end
if it returns a cursor). These calls test the repository evidence service and the live change under
review. After they complete, reply with exactly two lines and nothing else:
SMOKE-OK
COUNTER=1
'@
    $start = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review'
        arguments = @{
            instructions  = $firstInstructions
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
    if ($Reviewer -eq 'codex') {
        Assert-That 'fresh Codex turn used all requested evidence operations' `
            ($resultText -match 'evidence service completed (?:[4-9]|[1-9][0-9]+) tool call') $resultText
    }
    Assert-That 'review body is delimited' ($resultText -match 'BEGIN REVIEW') $resultText
    Assert-That 'the wait emitted MCP progress notifications' `
        ($script:progressMessages.Count -gt $progressBefore)

    # Issue #73: a client that reads only `structuredContent` must not be poorer than one reading the
    # text. This is the end-to-end check that the envelope actually carries the review -- the unit
    # tests pin the composition, but only a real turn proves the reviewer's own words arrive.
    $sc = $collected.result.structuredContent
    Assert-That 'the completed result carries structuredContent' ($null -ne $sc) $resultText
    Assert-That 'the envelope is at the current schema version' ($sc.schema_version -eq 3) `
        "schema_version=$($sc.schema_version)"
    Assert-That 'a turn that ran carries its prose on the structured channel' `
        ($sc.review_prose -is [string]) "review_prose=$($sc.review_prose)"
    Assert-That 'the structured prose is the reviewer''s own words' `
        ($sc.review_prose -match 'SMOKE-OK') $sc.review_prose
    Assert-That 'the result-context group is present' `
        (($sc.PSObject.Properties.Name -contains 'captured') -and
         ($sc.PSObject.Properties.Name -contains 'denial_count') -and
         ($sc.PSObject.Properties.Name -contains 'denial_count_is_floor') -and
         ($sc.PSObject.Properties.Name -contains 'resumable') -and
         ($sc.PSObject.Properties.Name -contains 'warnings')) `
        ($sc.PSObject.Properties.Name -join ',')
    Assert-That 'a turn that ran names the reviewer that ran it' `
        ($sc.reviewer -is [string]) "reviewer=$($sc.reviewer)"
    Write-Host $resultText

    Write-Host "`n=== 5. resuming the same review session ===" -ForegroundColor Cyan
    $resumeInstructions = @'
Still the smoke test. Call repository_scope, repository_read for README.md lines 1-5, and
repository_diff with base "branch-base" and head "worktree" (page it to the end if it returns a
cursor); do not use the shell. Then increment the counter you reported before and reply with
exactly two lines:
SMOKE-OK
COUNTER=2
'@
    $resume = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review'
        arguments = @{
            instructions = $resumeInstructions
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
    if ($Reviewer -eq 'codex') {
        Assert-That 'resumed Codex turn recreated and used the evidence service' `
            ($resumeResult -match 'evidence service completed (?:[2-9]|[1-9][0-9]+) tool call') $resumeResult
    }
    Assert-That 'the resumed wait emitted MCP progress notifications' `
        ($script:progressMessages.Count -gt $resumeProgressBefore)
    Write-Host $resumeResult

    Write-Host "`n=== 5b. a consult (second pair of eyes) ===" -ForegroundColor Cyan
    # The consult is a distinct protocol path -- its own prompt (no findings block), its own result
    # tool, and a prose answer rather than a findings envelope -- so a change touching it needs the
    # real round trip, not just the review above. Both reviewers exercise the evidence service here.
    $consultQuestion = @'
This is an automated smoke test of a consult tool. Do not review any code. Do not use the shell.
Call repository_scope and repository_search for the literal "cross-review" to exercise the evidence
service, then answer with exactly two lines and nothing else:
CONSULT-OK
LOOKS-FINE
'@
    $cstart = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_consult'
        arguments = @{ question = $consultQuestion; session = 'smoke-consult' }
    } -TimeoutSeconds 60
    $cstartText = Get-ToolText $cstart
    Assert-That 'consult start is not an error' ($cstart.result.isError -eq $false) $cstartText
    Assert-That 'consult announces itself as a consult' ($cstartText -match 'Consult started') $cstartText
    $consultId = ([regex]::Match($cstartText, 'review_id:\s*(\S+)')).Groups[1].Value
    Assert-That 'a consult review_id was returned' (-not [string]::IsNullOrWhiteSpace($consultId)) $cstartText
    Write-Host "  consult review_id: $consultId"

    # Cross-kind guard: a consult id must be refused by the review result tool, before any wait, and
    # the refusal must name the right tool. This costs no model call (it rejects on the kind check).
    $wrongTool = Send-Rpc -Method 'tools/call' -Params @{
        name = 'cross_model_review_result'; arguments = @{ review_id = $consultId; wait_seconds = 0 }
    } -TimeoutSeconds 30
    $wrongToolText = Get-ToolText $wrongTool
    Assert-That 'a consult id is refused by the review result tool, naming the right one' `
        (($wrongTool.result.isError -eq $true) -and ($wrongToolText -match 'cross_model_consult_result')) $wrongToolText

    Write-Host '  waiting for the consult...' -ForegroundColor DarkGray
    $cCollected = $null
    $consultProgressBefore = $script:progressMessages.Count
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        $cCollected = Send-Rpc -Method 'tools/call' -Params @{
            name      = 'cross_model_consult_result'
            arguments = @{ review_id = $consultId; wait_seconds = 120 }
            _meta     = @{ progressToken = "smoke-consult-$attempt" }
        } -TimeoutSeconds 300
        $text = Get-ToolText $cCollected
        if ($text -notmatch 'status:\s+running') { break }
        Write-Host "  still running (poll $attempt)" -ForegroundColor DarkGray
    }
    $cResult = Get-ToolText $cCollected
    Assert-That 'consult completed without error' ($cCollected.result.isError -eq $false) $cResult
    Assert-That 'consult reports completed status' ($cResult -match 'status:\s+completed') $cResult
    Assert-That 'the consult actually answered' ($cResult -match 'CONSULT-OK') $cResult
    Assert-That 'the consult body is delimited as an answer, not a review' ($cResult -match 'BEGIN ANSWER') $cResult
    # A consult certifies nothing: it renders no findings envelope or verdict block.
    Assert-That 'a consult carries no review-verdict block' ($cResult -notmatch 'BEGIN REVIEW') $cResult

    # Issue #73 parity for the consult envelope: a structured-only client gets the answer and facts.
    $csc = $cCollected.result.structuredContent
    Assert-That 'the consult carries structuredContent' ($null -ne $csc) $cResult
    Assert-That 'the structured consult is marked kind=consult' ($csc.kind -eq 'consult') "kind=$($csc.kind)"
    Assert-That 'the consult answer is on the structured channel' ($csc.answer -match 'CONSULT-OK') $csc.answer
    Assert-That 'the consult carries the denial-count floor field' `
        ($csc.PSObject.Properties.Name -contains 'denial_count_is_floor') ($csc.PSObject.Properties.Name -join ',')
    # No findings/convergence machinery on a consult.
    Assert-That 'the consult reports no verdict/outcome/findings' `
        (-not (($csc.PSObject.Properties.Name -contains 'outcome') -or
               ($csc.PSObject.Properties.Name -contains 'findings') -or
               ($csc.PSObject.Properties.Name -contains 'converged'))) ($csc.PSObject.Properties.Name -join ',')
    Assert-That 'the consult wait emitted MCP progress notifications' `
        ($script:progressMessages.Count -gt $consultProgressBefore)
    Write-Host $cResult

    Write-Host "`n=== 5c. a change-capturing consult (include_change: true) ===" -ForegroundColor Cyan
    # include_change: true runs a consult through the same spawn + evidence service a review uses --
    # which AGENTS.md classifies as protocol needing the real round trip. For git the change is
    # derived live through repository_diff (no static capture); a consult is informal and un-gated, so
    # this asserts the path runs end-to-end and the envelope is well-formed rather than an exact diff.
    $icQuestion = @'
This is an automated smoke test of a consult that includes the change. Do not use the shell.
Answer with exactly one line and nothing else:
INCLUDE-CHANGE-OK
'@
    $icStart = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_consult'
        arguments = @{ question = $icQuestion; session = 'smoke-consult-change'; include_change = $true }
    } -TimeoutSeconds 60
    $icStartText = Get-ToolText $icStart
    Assert-That 'include_change consult start is not an error' ($icStart.result.isError -eq $false) $icStartText
    $icId = ([regex]::Match($icStartText, 'review_id:\s*(\S+)')).Groups[1].Value
    Assert-That 'an include_change consult review_id was returned' (-not [string]::IsNullOrWhiteSpace($icId)) $icStartText
    Write-Host "  include_change consult review_id: $icId"

    Write-Host '  waiting for the include_change consult...' -ForegroundColor DarkGray
    $icCollected = $null
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        $icCollected = Send-Rpc -Method 'tools/call' -Params @{
            name      = 'cross_model_consult_result'
            arguments = @{ review_id = $icId; wait_seconds = 120 }
        } -TimeoutSeconds 300
        $text = Get-ToolText $icCollected
        if ($text -notmatch 'status:\s+running') { break }
        Write-Host "  still running (poll $attempt)" -ForegroundColor DarkGray
    }
    $icResult = Get-ToolText $icCollected
    Assert-That 'include_change consult completed without error' ($icCollected.result.isError -eq $false) $icResult
    Assert-That 'include_change consult reports completed status' ($icResult -match 'status:\s+completed') $icResult
    Assert-That 'the include_change consult actually answered' ($icResult -match 'INCLUDE-CHANGE-OK') $icResult
    $icsc = $icCollected.result.structuredContent
    Assert-That 'the include_change consult is marked kind=consult' ($icsc.kind -eq 'consult') "kind=$($icsc.kind)"
    # The session must be stamped include_change: true, so a resume that flips the capture mode is
    # refused. This is what the capture-contract binding exists to protect (issue #105).
    Write-Host $icResult

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
            instructions = 'Smoke test of poll cancellation. Call repository_change, then wait quietly; the poll will be cancelled.'
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
    # Anchor to the start of the response envelope. A running or completed collect begins with its
    # `status:` line; the reviewer body it may also contain (a completed review echoes the answer)
    # comes later, so `\A` keeps a body that happens to print "status: running" from being read as
    # the review's own status.
    Assert-That 'the poll cancellation left the review running or collectible' `
        (($afterText -match '\Astatus:\s+running') -or ($afterText -match '\Astatus:\s+completed')) $afterText
    # Whether the review was cancelled is decided by the response *envelope*, not by any word in it.
    # A genuinely cancelled review comes back as an isError failure carrying a `code: CANCELLED` line
    # (errors.rs); a running or completed review is not an isError, and its body -- which a completed
    # review appends verbatim and which could say anything, even "code: CANCELLED" -- cannot forge
    # that. The prompt above tells the reviewer the poll "will be cancelled", so a fast reviewer
    # echoes the word; guarding on isError is what stops that prose from failing a correct run.
    $afterCancelled = ($after.result.isError -eq $true) -and ($afterText -match 'code:\s+CANCELLED')
    Assert-That 'the poll cancellation did not cancel the review' (-not $afterCancelled) $afterText

    # Clean up so this review does not keep billing through the rest of the run.
    Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review_cancel'
        arguments = @{ review_id = $doomedId }
    } -TimeoutSeconds 30 | Out-Null

    Write-Host "`n=== 7b. cross_model_review_cancel stops the reviewer ===" -ForegroundColor Cyan
    $doomedB = Send-Rpc -Method 'tools/call' -Params @{
        name      = 'cross_model_review'
        arguments = @{
            instructions = 'Smoke test of explicit cancellation. Call repository_change, then wait quietly; this will be cancelled.'
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
    # Anchored to the start of the response: a still-running collect begins with `status: running`,
    # whereas a completed review that merely echoes "status: running" in its body would not, so the
    # anchor keeps reviewer prose from masking a review that really was still running.
    Assert-That 'the cancelled review is not still running' ($afterBText -notmatch '\Astatus:\s+running') $afterBText

    Start-Sleep -Milliseconds 500
    $evidenceChildren = @(Get-CimInstance Win32_Process -ErrorAction Stop | Where-Object {
        $_.CommandLine -and $_.CommandLine.Contains('--evidence-server') -and
        $_.CommandLine.Contains($stateDir)
    })
    Assert-That 'cancel and completion reaped every evidence service child' `
        ($evidenceChildren.Count -eq 0) (($evidenceChildren | Select-Object ProcessId,CommandLine | Out-String))

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
        # The review session is stamped 'review' (or a legacy null read as review); the consult
        # session is stamped 'consult', which is what keeps a cross-kind resume from crossing them.
        $consultSession = $saved.sessions.'smoke-consult'
        Assert-That 'the consult session was recorded with kind=consult' `
            ($null -ne $consultSession -and $consultSession.kind -eq 'consult') (Get-Content $sessionsFile -Raw)
        # The change-capturing consult stamps its capture contract (include_change: true) on the
        # record, so a later resume that flipped the capture mode would be refused (issue #105).
        $icSession = $saved.sessions.'smoke-consult-change'
        Assert-That 'the include_change consult recorded its capture contract' `
            ($null -ne $icSession -and $icSession.kind -eq 'consult' -and $icSession.include_change -eq $true) `
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
    if ($Reviewer -eq 'codex') {
        Assert-That 'Codex smoke had zero shell policy denials' `
            ($stderrText -notmatch 'rejected: blocked by policy') $stderrText
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
