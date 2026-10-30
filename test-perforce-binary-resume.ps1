<#
.SYNOPSIS
  Regression for #184 using synthetic files in a disposable local Perforce server.
  A pending changelist with text edits plus a binary edit, add and delete must still record a
  Full resume baseline, so the next turn collapses the unchanged text files instead of falling
  back to a full re-capture. The capture must also be reported complete, naming the binary files,
  with no "incomplete" warning (#189), and the same holds once the changelist is shelved and once
  it is submitted (#194). Requires p4, p4d and cargo. No model calls or access to an existing
  depot.
#>
[CmdletBinding()]
param([string]$P4d = 'C:\Program Files\Perforce\DVCS\p4d.exe')

$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ("cross-review-binresume-" + [guid]::NewGuid())
$server = Join-Path $root 'server'
$workspace = Join-Path $root 'workspace'
New-Item -ItemType Directory -Path $server, $workspace | Out-Null
$saved = @{}
$settings = @{
    P4USER = 'binresume-test'; P4CLIENT = 'binresume-test'; P4CHARSET = 'utf8'; P4PASSWD = ''
    P4CONFIG = 'cross-review-no-config-' + [guid]::NewGuid()
    P4ENVIRO = (Join-Path $root 'p4enviro')
    P4TICKETS = (Join-Path $root 'p4tickets.txt')
    CROSS_REVIEW_P4_TEST_CWD = $workspace
    CROSS_REVIEW_P4_TEST_CL = ''
    CROSS_REVIEW_P4_TEST_SHELVED = ''
    CROSS_REVIEW_P4_TEST_BINARY_ONLY = '1'
}
# Ask Windows for a free loopback port. The server must start successfully before use.
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start()
$settings.P4PORT = "127.0.0.1:$($listener.LocalEndpoint.Port)"
$listener.Stop()
$process = $null
# Windows PowerShell writes a native command's piped stdin in [Console]::InputEncoding. On a
# host where that is UTF-8 with a BOM, p4 reads the BOM as part of the first spec field
# ("Unknown field name" for Client:) or of a password ("Password invalid.").
$savedInputEncoding = [Console]::InputEncoding
function Invoke-TestP4 {
    $result = @($input | & p4 @args)
    if ($LASTEXITCODE -ne 0) { throw "p4 $args failed ($LASTEXITCODE)" }
    $result
}
try {
    [Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
    foreach ($name in $settings.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $settings[$name], 'Process')
    }
    & $P4d -r $server -xi | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not initialize the Unicode test server.' }
    # p4d 2026.1 is secure by default; restore the older user defaults so the test user is
    # created, sets its own password and logs in (see test-perforce-line-endings.ps1).
    foreach ($configurable in 'dm.user.noautocreate=0', 'dm.user.resetpassword=0', 'dm.user.setinitialpasswd=1') {
        & $P4d -r $server "-cset $configurable" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "Could not set $configurable on the test server." }
    }
    $process = Start-Process -FilePath $P4d -ArgumentList @('-r', "`"$server`"", '-p', $settings.P4PORT, '-L', "`"$(Join-Path $root 'server.log')`"") -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 1
    if ($process.HasExited) { throw 'The disposable p4d did not start.' }
    $password = 'Bin-' + [guid]::NewGuid().ToString('N').Substring(0, 12) + '-7a'
    "$password`n$password" | Invoke-TestP4 passwd | Out-Null
    $password | Invoke-TestP4 login | Out-Null
    @"
Client: binresume-test
Owner: binresume-test
Root: $workspace
View:
    //depot/... //binresume-test/...
"@ | Invoke-TestP4 client -i | Out-Null
    $utf8 = [Text.UTF8Encoding]::new($false)
    # Diffs large enough that collapsing them outweighs the follow-up framing turn 2 adds.
    $lines = { param($word) ((1..300 | ForEach-Object { "$word line $_" }) -join "`n") + "`n" }
    [IO.File]::WriteAllText((Join-Path $workspace 'a.txt'), (& $lines 'alpha'), $utf8)
    [IO.File]::WriteAllText((Join-Path $workspace 'b.txt'), (& $lines 'beta'), $utf8)
    [IO.File]::WriteAllBytes((Join-Path $workspace 'tool.exe'), [byte[]](0x4d, 0x5a, 0, 1, 2, 3))
    [IO.File]::WriteAllBytes((Join-Path $workspace 'gone.bin'), [byte[]](0, 9, 8, 7))
    Invoke-TestP4 add -t text "$workspace\a.txt" "$workspace\b.txt" | Out-Null
    Invoke-TestP4 add -t binary "$workspace\tool.exe" "$workspace\gone.bin" | Out-Null
    Invoke-TestP4 submit -d 'Synthetic baseline' | Out-Null
    $created = @"
Change: new
Client: binresume-test
User: binresume-test
Description:
    Synthetic binary-resume regression
"@ | Invoke-TestP4 change -i
    if (($created -join '') -notmatch 'Change (\d+) created') { throw 'No pending changelist was created.' }
    $env:CROSS_REVIEW_P4_TEST_CL = $Matches[1]
    Invoke-TestP4 edit -c $env:CROSS_REVIEW_P4_TEST_CL "$workspace\a.txt" "$workspace\b.txt" "$workspace\tool.exe" | Out-Null
    Invoke-TestP4 delete -c $env:CROSS_REVIEW_P4_TEST_CL "$workspace\gone.bin" | Out-Null
    [IO.File]::WriteAllText((Join-Path $workspace 'a.txt'), (& $lines 'ALPHA'), $utf8)
    [IO.File]::WriteAllText((Join-Path $workspace 'b.txt'), (& $lines 'BETA'), $utf8)
    [IO.File]::WriteAllBytes((Join-Path $workspace 'tool.exe'), [byte[]](0x4d, 0x5a, 0, 4, 5, 6))
    [IO.File]::WriteAllBytes((Join-Path $workspace 'new.bin'), [byte[]](0, 1, 0, 1))
    Invoke-TestP4 add -c $env:CROSS_REVIEW_P4_TEST_CL -t binary "$workspace\new.bin" | Out-Null

    # Two captures of the same changelist through the real capture path: turn 1 records the
    # baseline and turn 2 resumes against it. Before #184 turn 1 recorded Disabled, and the test
    # printed its SKIP line instead of asserting the delta.
    function Invoke-ResumeTest([string]$Basis) {
        Push-Location $PSScriptRoot
        try {
            $ErrorActionPreference = 'Continue' # cargo's ordinary progress is on stderr
            $output = (& cargo test live_resume_delta_collapses_unchanged_files -- --nocapture 2>&1 | Out-String)
            $testExit = $LASTEXITCODE
            $ErrorActionPreference = 'Stop'
        } finally { Pop-Location }
        if ($testExit -ne 0) { throw "Resume test ($Basis) failed:`n$output" }
        if ($output.Contains('SKIP delta assertion')) { throw "Turn 1 ($Basis) recorded a Disabled baseline:`n$output" }
        if (-not $output.Contains('resume delta: turn 1 =')) { throw "The resume test ($Basis) did not run:`n$output" }
        if (-not $output.Contains('binary-only capture:')) { throw "The completeness check ($Basis) did not run:`n$output" }
        Write-Host "PASS ($Basis): binary files leave the resume baseline Full, so unchanged text files collapse on the next turn."
        Write-Host "PASS ($Basis): a changelist short only of binary files is reported complete, with no incomplete warning."
    }
    Invoke-ResumeTest 'pending'

    # The same changelist shelved and reverted, so only its shelf is reviewable, then submitted:
    # those paths learn a file's type from `describe` rather than `opened` (#194).
    $cl = $env:CROSS_REVIEW_P4_TEST_CL
    Invoke-TestP4 shelve -c $cl | Out-Null
    Invoke-TestP4 revert -w -c $cl "//binresume-test/..." | Out-Null
    $env:CROSS_REVIEW_P4_TEST_SHELVED = '1'
    Invoke-ResumeTest 'shelved'
    Remove-Item Env:CROSS_REVIEW_P4_TEST_SHELVED
    Invoke-TestP4 unshelve -s $cl -c $cl | Out-Null
    Invoke-TestP4 shelve -d -c $cl | Out-Null
    $submitted = (Invoke-TestP4 submit -c $cl) -join "`n"
    if ($submitted -notmatch 'Change (\d+) submitted') { throw "Submit failed: $submitted" }
    $env:CROSS_REVIEW_P4_TEST_CL = $Matches[1]
    Invoke-ResumeTest 'submitted'
} finally {
    if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id; $process.WaitForExit() }
    [Console]::InputEncoding = $savedInputEncoding
    foreach ($name in $saved.Keys) {
        if ($null -eq $saved[$name]) {
            Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
        }
    }
    # Delete only the unique directory created above, after checking its parent and name.
    $resolved = [IO.Path]::GetFullPath($root)
    if ([IO.Path]::GetDirectoryName($resolved).TrimEnd('\') -eq [IO.Path]::GetTempPath().TrimEnd('\') -and
        [IO.Path]::GetFileName($resolved).StartsWith('cross-review-binresume-')) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
