<#
.SYNOPSIS
  Interactive end-to-end check of reviewer-profile provisioning (cross_model_setup_profile).

.DESCRIPTION
  Drives cross-review.exe over stdio exactly as an MCP client would and calls
  cross_model_setup_profile with login:true, so a real vendor sign-in is performed. This
  spends NO model tokens (setup runs the vendor login, it does not call a model), but it IS
  interactive: a browser opens for you to APPROVE and then SIGN IN, and — for a vendor/account
  whose login shows a code — a small local page opens for you to paste that code.

  Everything is isolated under a throwaway CROSS_REVIEW_HOME (a temp directory), so it never
  touches your real profile store or your ambient (desktop-app) login. The provisioned profile
  home is a dedicated directory under that temp CROSS_REVIEW_HOME either way.

  Unit tests cover the loopback page, URL extraction and framing; this covers what only a real
  CLI can prove: that the vendor login runs, credentials land in the dedicated home, and the
  repository is authorized. It complements smoke.ps1, which exercises the *review* path.

  The account that gets provisioned is whatever the browser OAuth resolves to. To exercise a
  specific account (e.g. to hit the code-paste path), sign into that account in the browser
  first, or use a separate browser profile. Run once per (reviewer, account) you want to cover.

.EXAMPLE
  .\smoke-profile.ps1 -Reviewers codex,claude
.EXAMPLE
  # Provision Claude under whichever account the browser is signed into, keep the home to inspect.
  .\smoke-profile.ps1 -Reviewers claude -Profile work -Keep
#>
[CmdletBinding()]
param(
    # Which reviewer families to provision, in order.
    [ValidateSet('codex', 'claude')]
    [string[]]$Reviewers = @('codex', 'claude'),

    # The profile name to provision (a safe name: letters, digits, '.', '_', '-').
    [string]$Profile = 'smoke',

    # Path to the built server. Point at target\release so it need not be the (possibly locked) dist copy.
    [string]$Exe = (Join-Path $PSScriptRoot 'target\release\cross-review.exe'),

    # How long to wait for you to complete each sign-in.
    [int]$LoginTimeoutSeconds = 600,

    # An explicit CROSS_REVIEW_HOME to use instead of a throwaway temp dir (also implies -Keep).
    [string]$HomeDir,

    # Keep the CROSS_REVIEW_HOME (with the provisioned credentials) for inspection instead of deleting it.
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Exe)) { throw "cross-review.exe not found at $Exe. Run .\build.ps1 (or cargo build --release) first." }

if ($HomeDir) { $crHome = $HomeDir; $Keep = $true } else { $crHome = Join-Path ([System.IO.Path]::GetTempPath()) "cr-smoke-home-$PID" }
$stateDir = Join-Path ([System.IO.Path]::GetTempPath()) "cr-smoke-state-$PID"
New-Item -ItemType Directory -Force $crHome | Out-Null

Write-Host "==> CROSS_REVIEW_HOME = $crHome" -ForegroundColor Cyan
Write-Host "==> provisioning profile '$Profile' for: $($Reviewers -join ', ')" -ForegroundColor Cyan

# The server's own --reviewer does not matter here: cross_model_setup_profile takes the reviewer as an
# argument. A minimal valid config launches the server; setup resolves the vendor CLI per its argument.
$serverArgs = @('--reviewer', 'codex', '--level', 'smoke:gpt-5.6-luna:low', '--state-dir', $stateDir)

# Windows PowerShell 5.1's ProcessStartInfo has no ArgumentList; quote by hand (paths here are simple).
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Exe
$psi.Arguments = ($serverArgs -join ' ')
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.WorkingDirectory = $PSScriptRoot
$psi.EnvironmentVariables['CROSS_REVIEW_HOME'] = $crHome
$proc = [System.Diagnostics.Process]::Start($psi)

# BOM-less UTF-8 stdin (a leading BOM is not valid JSON).
$stdin = New-Object System.IO.StreamWriter($proc.StandardInput.BaseStream, (New-Object System.Text.UTF8Encoding($false)))
$stdin.AutoFlush = $true

# Surface the server's approval / code-page URLs live so you know where to act.
$stderrBuffer = New-Object System.Text.StringBuilder
$stderrSub = Register-ObjectEvent -InputObject $proc -EventName ErrorDataReceived -Action {
    if ($EventArgs.Data) {
        [void]$Event.MessageData.AppendLine($EventArgs.Data)
        if ($EventArgs.Data -match 'https?://127\.0\.0\.1|Approve|paste') {
            Write-Host "  [server] $($EventArgs.Data)" -ForegroundColor Yellow
        }
    }
} -MessageData $stderrBuffer
$proc.BeginErrorReadLine()

$script:nextId = 0
$script:pending = $null
function Read-Line([int]$TimeoutSeconds) {
    if (-not $script:pending) { $script:pending = $proc.StandardOutput.ReadLineAsync() }
    if (-not $script:pending.Wait([TimeSpan]::FromSeconds($TimeoutSeconds))) { return $null }
    $line = $script:pending.Result; $script:pending = $null
    if ($null -eq $line) { throw 'server closed stdout' }
    return $line
}
function Send-Rpc([string]$Method, $Params, [switch]$Notification, [int]$TimeoutSeconds = 60) {
    $m = [ordered]@{ jsonrpc = '2.0'; method = $Method }
    if (-not $Notification) { $script:nextId++; $m['id'] = $script:nextId }
    if ($null -ne $Params) { $m['params'] = $Params }
    $stdin.WriteLine(($m | ConvertTo-Json -Depth 12 -Compress)); $stdin.Flush()
    if ($Notification) { return $null }
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ($true) {
        $rem = [int][Math]::Ceiling(($deadline - [DateTime]::UtcNow).TotalSeconds)
        if ($rem -le 0) { throw "timed out after ${TimeoutSeconds}s waiting for '$Method'" }
        $line = Read-Line $rem
        if ($null -eq $line) { throw "timed out after ${TimeoutSeconds}s waiting for '$Method'" }
        $p = $line | ConvertFrom-Json
        if ($p.method -eq 'notifications/progress') { continue }
        if ($p.id -ne $m['id']) { throw "response id $($p.id) does not answer '$Method' (id $($m['id'])): $line" }
        return $p
    }
}
function Get-ToolText($r) { if ($null -eq $r.result) { return '' } ($r.result.content | ForEach-Object { $_.text }) -join "`n" }

$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$Name, [bool]$Cond, [string]$Detail) {
    if ($Cond) { Write-Host "  PASS  $Name" -ForegroundColor Green }
    else { Write-Host "  FAIL  $Name" -ForegroundColor Red; if ($Detail) { Write-Host "        $Detail" -ForegroundColor DarkYellow }; $failures.Add($Name) }
}

try {
    Write-Host "`n=== initialize ===" -ForegroundColor Cyan
    $init = Send-Rpc 'initialize' @{ protocolVersion = '2025-06-18'; capabilities = @{}; clientInfo = @{ name = 'smoke-profile'; version = '1' } } -TimeoutSeconds 30
    Check 'server identifies as cross-review' ($init.result.serverInfo.name -eq 'cross-review') "$($init.result.serverInfo.name)"
    Send-Rpc 'notifications/initialized' -Notification | Out-Null

    $list = Send-Rpc 'tools/list' $null -TimeoutSeconds 30
    $names = @($list.result.tools | ForEach-Object { $_.name })
    Check 'setup tool is exposed' ($names -contains 'cross_model_setup_profile') "tools: $($names -join ', ')"

    foreach ($rv in $Reviewers) {
        Write-Host "`n=== provision '$rv' profile '$Profile' via real login ===" -ForegroundColor Cyan
        Write-Host "  ACTION: a browser tab opens to APPROVE, then to SIGN IN." -ForegroundColor Magenta
        Write-Host "          If the sign-in shows a CODE, paste it on the local page that opens; otherwise it completes in the browser." -ForegroundColor Magenta
        Write-Host "          Waiting up to $LoginTimeoutSeconds s ..." -ForegroundColor DarkGray

        $resp = Send-Rpc 'tools/call' @{
            name      = 'cross_model_setup_profile'
            arguments = @{ reviewer = $rv; profile = $Profile; login = $true }
        } -TimeoutSeconds $LoginTimeoutSeconds
        $text = Get-ToolText $resp
        Write-Host "  -> $text"
        Check "$rv setup did not error" ($resp.result.isError -eq $false) $text
        Check "$rv result reports provisioned+authorized" ($text -match 'Provisioned and authorized') $text

        $homeDir = Join-Path $crHome "profiles\$rv\$Profile"
        Check "$rv profile home exists" (Test-Path $homeDir) $homeDir
        $accountFile = if ($rv -eq 'codex') { 'auth.json' } else { '.claude.json' }
        Check "$rv account file '$accountFile' landed in the dedicated home" (Test-Path (Join-Path $homeDir $accountFile)) $homeDir

        # Verify the allowlist by parsing the JSON (its paths are backslash-escaped, so a raw regex
        # match on the text is unreliable): look for an entry for this reviewer whose effective_home is
        # the dedicated profile home.
        $allow = Join-Path $crHome 'auth\allowlist.json'
        $allowText = if (Test-Path $allow) { Get-Content $allow -Raw } else { '' }
        $allowOk = $false
        if ($allowText.Trim()) {
            try {
                $entries = ($allowText | ConvertFrom-Json).entries
                $allowOk = @($entries | Where-Object {
                        $_.reviewer_family -eq $rv -and $_.effective_home -like "*profiles\$rv\$Profile"
                    }).Count -gt 0
            }
            catch {}
        }
        Check "$rv allowlist entry recorded" $allowOk $allowText
    }
}
finally {
    if ($proc -and -not $proc.HasExited) { $stdin.Close(); if (-not $proc.WaitForExit(5000)) { $proc.Kill() } }
    if ($stderrSub) { Unregister-Event -SubscriptionId $stderrSub.Id -ErrorAction SilentlyContinue }
    $err = $stderrBuffer.ToString()
    if ($err.Trim()) {
        Write-Host "`n--- server stderr (tail) ---" -ForegroundColor DarkGray
        ($err -split "`n" | Select-Object -Last 20) | ForEach-Object { Write-Host $_ -ForegroundColor DarkGray }
    }
    Remove-Item $stateDir -Recurse -Force -ErrorAction SilentlyContinue
    if ($Keep) {
        Write-Host "`n(kept CROSS_REVIEW_HOME for inspection: $crHome)" -ForegroundColor DarkGray
    }
    else {
        # The home holds real credentials for the provisioned test profile; remove it unless -Keep.
        try { [System.IO.Directory]::Delete($crHome, $true) } catch { Write-Host "(could not remove $crHome : $_)" -ForegroundColor DarkYellow }
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "PROFILE SMOKE PASSED (reviewers: $($Reviewers -join ', '); profile: $Profile)" -ForegroundColor Green
    exit 0
}
Write-Host "PROFILE SMOKE FAILED: $($failures.Count) check(s)" -ForegroundColor Red
$failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
exit 1
