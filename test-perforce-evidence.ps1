<#
.SYNOPSIS
  Live check of the Perforce evidence tools (issue #183) on a disposable local Perforce server.
  repository_history, repository_revision and repository_diff must list a file's submitted
  changelists, read it as of one, and diff it across changelists and against the workspace.
  A pending changelist must be refused rather than read as the submitted revision below it,
  and a shelved one read as its shelf (issue #188).
  Requires p4, p4d and cargo. No model calls or access to an existing depot.
#>
[CmdletBinding()]
param([string]$P4d = 'C:\Program Files\Perforce\DVCS\p4d.exe')

$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ("cross-review-p4evidence-" + [guid]::NewGuid())
$server = Join-Path $root 'server'
$workspace = Join-Path $root 'workspace'
New-Item -ItemType Directory -Path $server, $workspace | Out-Null
$saved = @{}
$settings = @{
    P4USER = 'p4evidence-test'; P4CLIENT = 'p4evidence-test'; P4CHARSET = 'utf8'; P4PASSWD = ''
    P4CONFIG = 'cross-review-no-config-' + [guid]::NewGuid()
    P4ENVIRO = (Join-Path $root 'p4enviro')
    P4TICKETS = (Join-Path $root 'p4tickets.txt')
    CROSS_REVIEW_P4_TEST_CWD = $workspace
    CROSS_REVIEW_P4_TEST_EVIDENCE = ''
    CROSS_REVIEW_P4_TEST_SHELVED = ''
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
    $password = 'Evi-' + [guid]::NewGuid().ToString('N').Substring(0, 12) + '-7a'
    "$password`n$password" | Invoke-TestP4 passwd | Out-Null
    $password | Invoke-TestP4 login | Out-Null
    @"
Client: p4evidence-test
Owner: p4evidence-test
Root: $workspace
View:
    //depot/... //p4evidence-test/...
"@ | Invoke-TestP4 client -i | Out-Null
    $utf8 = [Text.UTF8Encoding]::new($false)
    $file = Join-Path $workspace 'a.txt'
    function Submit-Test([string]$Description) {
        $out = (Invoke-TestP4 submit -d $Description) -join "`n"
        if ($out -notmatch 'Change (\d+) submitted') { throw "Submit failed: $out" }
        $Matches[1]
    }
    [IO.File]::WriteAllText($file, "version one`n", $utf8)
    $other = Join-Path $workspace 'b.txt'
    [IO.File]::WriteAllText($other, "bee`n", $utf8)
    Invoke-TestP4 add -t text $file $other | Out-Null
    $first = Submit-Test 'First edit'
    Invoke-TestP4 edit $file | Out-Null
    [IO.File]::WriteAllText($file, "version two`n", $utf8)
    $second = Submit-Test 'Second edit'
    function New-TestChange([string]$Description) {
        $out = ("Change: new`nDescription:`n`t$Description`n" | Invoke-TestP4 change -i) -join "`n"
        if ($out -notmatch 'Change (\d+) created') { throw "Change failed: $out" }
        $Matches[1]
    }
    # A pending edit in a numbered changelist, so the workspace differs from both submitted
    # revisions and the changelist has no revision on the server.
    $pending = New-TestChange 'Pending edit'
    Invoke-TestP4 edit -c $pending $file | Out-Null
    [IO.File]::WriteAllText($file, "version three`n", $utf8)
    # A shelved edit of the other file.
    $shelved = New-TestChange 'Shelved edit'
    Invoke-TestP4 edit -c $shelved $other | Out-Null
    [IO.File]::WriteAllText($other, "bee shelved`n", $utf8)
    Invoke-TestP4 shelve -c $shelved | Out-Null
    $env:CROSS_REVIEW_P4_TEST_EVIDENCE = "$first,$second,$pending,$shelved"

    Push-Location $PSScriptRoot
    try {
        $ErrorActionPreference = 'Continue' # cargo's ordinary progress is on stderr
        $output = (& cargo test live_perforce_history_revision_and_diff -- --nocapture 2>&1 | Out-String)
        $testExit = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
    } finally { Pop-Location }
    if ($testExit -ne 0) { throw "Evidence test failed:`n$output" }
    if (-not $output.Contains('PASS: live Perforce history, revision and diff')) { throw "The evidence test did not run:`n$output" }
    Write-Host 'PASS: Perforce history, revision and diff evidence read the right changelists, content and diffs.'
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
        [IO.Path]::GetFileName($resolved).StartsWith('cross-review-p4evidence-')) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
