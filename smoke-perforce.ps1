<#
.SYNOPSIS
  End-to-end check of the Perforce evidence tools (issue #183) through a real reviewer.

.DESCRIPTION
  Starts a disposable local Perforce server and workspace, then runs one real review of a pending
  changelist through target\release\cross-review.exe over MCP, as smoke.ps1 does for git. The marker
  the reviewer must quote exists only in an older submitted revision of the file: not in the
  workspace, and not in the change under review. So the reviewer can only quote it if
  repository_history and repository_revision worked from the evidence server the reviewer CLI
  started, with the p4 settings the parent handed it. This calls the reviewer model for real, so it
  costs tokens. Requires p4, p4d and a release build (cargo build --release).

  The workspace lives at a fixed path, %LOCALAPPDATA%\cross-review-p4smoke\workspace, because a
  Claude account profile is authorized per
  working directory. Run once with -AuthorizeProfile to approve the profile for that path in your
  browser; later runs reuse the approval.

.EXAMPLE
  .\smoke-perforce.ps1 -Reviewer codex
  .\smoke-perforce.ps1 -Reviewer claude -AuthorizeProfile
  .\smoke-perforce.ps1 -Reviewer claude
#>
[CmdletBinding()]
param(
    [ValidateSet('codex', 'claude')]
    [string]$Reviewer = 'codex',
    [string]$ClaudeProfile = 'work',
    [string]$Exe,
    [string]$P4d = 'C:\Program Files\Perforce\DVCS\p4d.exe',
    # Claude only: open the human approval page that authorizes -ClaudeProfile for the workspace.
    [switch]$AuthorizeProfile
)

$ErrorActionPreference = 'Stop'
if (-not $Exe) { $Exe = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) 'target\release\cross-review.exe' }
if (-not (Test-Path $Exe)) { throw "cross-review.exe not found at $Exe. Run cargo build --release first." }

# A fixed root, so a profile authorization for its workspace survives between runs. Only a root
# this script created (it holds the marker file) is ever cleared, and never through a reparse point.
$root = Join-Path $env:LOCALAPPDATA 'cross-review-p4smoke'
$markerFile = Join-Path $root '.cross-review-p4smoke'
function Test-Reparse([string]$Path) {
    (Test-Path -LiteralPath $Path) -and
    ((Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)
}
if ((Test-Path -LiteralPath $root) -and -not (Test-Path -LiteralPath $markerFile -PathType Leaf)) {
    throw "$root exists but was not created by smoke-perforce.ps1; remove it or move it aside."
}
if (Test-Reparse $root) { throw "$root is a reparse point; refusing to use it." }
$server = Join-Path $root 'server'
$workspace = Join-Path $root 'workspace'
$stateDir = Join-Path $root 'state'
function Clear-Run {
    $files = 'p4tickets.txt', 'p4enviro', 'server.log' | ForEach-Object { Join-Path $root $_ }
    foreach ($item in @($server, $workspace, $stateDir) + $files) {
        if (Test-Reparse $item) { throw "$item is a reparse point; refusing to delete through it." }
        if (Test-Path -LiteralPath $item) { Remove-Item -LiteralPath $item -Recurse -Force -ErrorAction SilentlyContinue }
    }
}
Clear-Run
New-Item -ItemType Directory -Path $server, $workspace, $stateDir -Force | Out-Null
New-Item -ItemType File -Path $markerFile -Force | Out-Null
$saved = @{}
$settings = @{
    P4USER = 'p4smoke-test'; P4CLIENT = 'p4smoke-test'; P4CHARSET = 'utf8'; P4PASSWD = ''
    P4CONFIG = 'cross-review-no-config-' + [guid]::NewGuid()
    P4ENVIRO = (Join-Path $root 'p4enviro')
    P4TICKETS = (Join-Path $root 'p4tickets.txt')
}
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start()
$settings.P4PORT = "127.0.0.1:$($listener.LocalEndpoint.Port)"
$listener.Stop()
$process = $null
$proc = $null
$failures = New-Object System.Collections.Generic.List[string]
# See test-perforce-line-endings.ps1: piped stdin must not carry a BOM into p4.
$savedInputEncoding = [Console]::InputEncoding
function Invoke-TestP4 {
    $result = @($input | & p4 @args)
    if ($LASTEXITCODE -ne 0) { throw "p4 $args failed ($LASTEXITCODE)" }
    $result
}
function Assert-That {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][bool]$Condition, [string]$Detail)
    if ($Condition) { Write-Host "  PASS  $Name" -ForegroundColor Green; return }
    Write-Host "  FAIL  $Name" -ForegroundColor Red
    if ($Detail) { Write-Host "        $Detail" -ForegroundColor DarkYellow }
    $failures.Add($Name)
}
$marker = "P4-SMOKE-$([guid]::NewGuid().ToString('N').Substring(0, 12))"
try {
    [Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
    foreach ($name in $settings.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $settings[$name], 'Process')
    }
    & $P4d -r $server -xi | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not initialize the Unicode test server.' }
    foreach ($configurable in 'dm.user.noautocreate=0', 'dm.user.resetpassword=0', 'dm.user.setinitialpasswd=1') {
        & $P4d -r $server "-cset $configurable" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "Could not set $configurable on the test server." }
    }
    $process = Start-Process -FilePath $P4d -ArgumentList @('-r', "`"$server`"", '-p', $settings.P4PORT, '-L', "`"$(Join-Path $root 'server.log')`"") -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 1
    if ($process.HasExited) { throw 'The disposable p4d did not start.' }
    $password = 'Smk-' + [guid]::NewGuid().ToString('N').Substring(0, 12) + '-7a'
    "$password`n$password" | Invoke-TestP4 passwd | Out-Null
    $password | Invoke-TestP4 login | Out-Null
    @"
Client: p4smoke-test
Owner: p4smoke-test
Root: $workspace
View:
    //depot/... //p4smoke-test/...
"@ | Invoke-TestP4 client -i | Out-Null

    # a.txt: the marker only in the first submitted revision; the pending change edits two -> three.
    $utf8 = [Text.UTF8Encoding]::new($false)
    $file = Join-Path $workspace 'a.txt'
    [IO.File]::WriteAllText($file, "version one $marker`n", $utf8)
    Invoke-TestP4 add -t text $file | Out-Null
    Invoke-TestP4 submit -d 'First edit' | Out-Null
    Invoke-TestP4 edit $file | Out-Null
    [IO.File]::WriteAllText($file, "version two`n", $utf8)
    Invoke-TestP4 submit -d 'Second edit' | Out-Null
    $created = "Change: new`nClient: p4smoke-test`nUser: p4smoke-test`nDescription:`n    Third edit`n" | Invoke-TestP4 change -i
    if (($created -join '') -notmatch 'Change (\d+) created') { throw 'No pending changelist was created.' }
    $pending = $Matches[1]
    Invoke-TestP4 edit -c $pending $file | Out-Null
    [IO.File]::WriteAllText($file, "version three`n", $utf8)

    $levelModel = if ($Reviewer -eq 'claude') { 'claude-opus-4-8' } else { 'gpt-5.6-luna' }
    $effort = if ($Reviewer -eq 'claude') { 'medium' } else { 'low' }
    $serverArgs = @('--reviewer', $Reviewer, '--level', "smoke:${levelModel}:${effort}",
        '--vcs', 'perforce', '--cwd', $workspace, '--state-dir', $stateDir, '--timeout-seconds', '600')
    if ($Reviewer -eq 'claude') { $serverArgs += @('--claude-profile', $ClaudeProfile) }
    Write-Host "==> launching: $Exe $($serverArgs -join ' ')" -ForegroundColor Cyan
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = ($serverArgs | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.WorkingDirectory = $workspace
    $proc = [System.Diagnostics.Process]::Start($psi)
    $stdin = New-Object System.IO.StreamWriter($proc.StandardInput.BaseStream, (New-Object System.Text.UTF8Encoding($false)))
    $stdin.AutoFlush = $true
    $stderrTask = $proc.StandardError.ReadToEndAsync()
    $script:nextId = 0
    function Send-Rpc {
        param([string]$Method, $Params, [switch]$Notification, [int]$TimeoutSeconds = 420)
        $message = [ordered]@{ jsonrpc = '2.0'; method = $Method }
        if (-not $Notification) { $script:nextId++; $message['id'] = $script:nextId }
        if ($null -ne $Params) { $message['params'] = $Params }
        $stdin.WriteLine(($message | ConvertTo-Json -Depth 12 -Compress))
        if ($Notification) { return $null }
        $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        while ($true) {
            $task = $proc.StandardOutput.ReadLineAsync()
            $remaining = ($deadline - [DateTime]::UtcNow)
            if ($remaining -le [TimeSpan]::Zero -or -not $task.Wait($remaining)) { throw "timed out waiting for '$Method'" }
            if ($null -eq $task.Result) { throw 'server closed stdout' }
            $parsed = $task.Result | ConvertFrom-Json
            if ($parsed.method -eq 'notifications/progress') { continue }
            if ($parsed.id -eq $message['id']) { return $parsed }
        }
    }
    function Get-ToolText($Response) {
        if ($null -eq $Response.result) { return '' }
        ($Response.result.content | ForEach-Object { $_.text }) -join "`n"
    }

    Send-Rpc -Method 'initialize' -Params @{
        protocolVersion = '2025-06-18'; capabilities = @{}; clientInfo = @{ name = 'smoke-perforce.ps1'; version = '1.0' }
    } -TimeoutSeconds 30 | Out-Null
    Send-Rpc -Method 'notifications/initialized' -Notification | Out-Null

    if ($AuthorizeProfile -and $Reviewer -eq 'claude') {
        Write-Host "==> approve profile '$ClaudeProfile' for $workspace in the browser page that opens" -ForegroundColor Yellow
        $setup = Send-Rpc -Method 'tools/call' -Params @{
            name = 'cross_model_setup_profile'; arguments = @{ reviewer = 'claude'; profile = $ClaudeProfile }
        } -TimeoutSeconds 900
        Write-Host (Get-ToolText $setup)
    }

    $instructions = @'
This is an automated smoke test of the Perforce evidence tools, not a real review. Do exactly this:
1. Call repository_history with path "a.txt".
2. Call repository_revision with id set to the OLDEST changelist id it listed, and path "a.txt".
   Its content contains one token that starts with "P4-SMOKE-"; copy it exactly.
3. Call repository_diff with path "a.txt" and base set to the NEWEST changelist id history listed.
Then reply with exactly two lines and nothing else:
P4-SMOKE-OK
MARKER=<the token from step 2>
'@
    $start = Send-Rpc -Method 'tools/call' -Params @{
        name = 'cross_model_review'; arguments = @{ instructions = $instructions; session = 'p4smoke'; change = $pending }
    } -TimeoutSeconds 120
    $startText = Get-ToolText $start
    $reviewId = ([regex]::Match($startText, 'review_id:\s*(\S+)')).Groups[1].Value
    Assert-That 'review started' (-not [string]::IsNullOrWhiteSpace($reviewId)) $startText
    if (-not $reviewId) { throw "The review did not start:`n$startText" }
    Write-Host '  waiting for the reviewer...' -ForegroundColor DarkGray
    $collected = $null
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        $collected = Send-Rpc -Method 'tools/call' -Params @{
            name = 'cross_model_review_result'; arguments = @{ review_id = $reviewId; wait_seconds = 120 }
        } -TimeoutSeconds 300
        if ((Get-ToolText $collected) -notmatch 'status:\s+running') { break }
    }
    $resultText = Get-ToolText $collected
    Assert-That 'review completed' ($resultText -match 'status:\s+completed') $resultText
    Assert-That 'the reviewer answered' ($resultText -match 'P4-SMOKE-OK') $resultText
    Assert-That 'the reviewer quoted the marker from an older revision' ($resultText.Contains("MARKER=$marker")) $resultText
} finally {
    if ($proc -and -not $proc.HasExited) {
        try { $stdin.Close() } catch {}
        if (-not $proc.WaitForExit(15000)) { $proc.Kill() }
    }
    if ($stderrTask -and $stderrTask.Wait(5000) -and $stderrTask.Result.Trim()) {
        Write-Host "`n--- server stderr ---" -ForegroundColor DarkGray
        Write-Host $stderrTask.Result -ForegroundColor DarkGray
    }
    if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id; $process.WaitForExit() }
    [Console]::InputEncoding = $savedInputEncoding
    foreach ($name in $saved.Keys) {
        if ($null -eq $saved[$name]) {
            Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
        }
    }
    # Keep the root and its marker, so the workspace path and its authorization persist.
    Clear-Run
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "PERFORCE SMOKE PASSED (reviewer: $Reviewer)" -ForegroundColor Green
    exit 0
}
Write-Host "PERFORCE SMOKE FAILED: $($failures.Count) check(s)" -ForegroundColor Red
$failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
exit 1
