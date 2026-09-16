<#
.SYNOPSIS
  Regression for #130 using synthetic files in a disposable local Perforce server.
  Requires p4, p4d and cargo. No model calls or access to an existing depot.
#>
[CmdletBinding()]
param([string]$P4d = 'C:\Program Files\Perforce\DVCS\p4d.exe')

$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ("cross-review-eol-" + [guid]::NewGuid())
$server = Join-Path $root 'server'
$workspace = Join-Path $root 'workspace'
New-Item -ItemType Directory -Path $server, $workspace | Out-Null
$saved = @{}
$settings = @{
    P4USER = 'eol-test'; P4CLIENT = 'eol-test'; P4CHARSET = 'utf8'; P4PASSWD = ''
    P4CONFIG = 'cross-review-no-config-' + [guid]::NewGuid()
    P4ENVIRO = (Join-Path $root 'p4enviro')
    P4DIFF = ''; P4DIFFUNICODE = ''
    CROSS_REVIEW_P4_TEST_CWD = $workspace
    CROSS_REVIEW_P4_TEST_CL = ''
    CROSS_REVIEW_P4_TEST_SHELVED = ''
}
# Ask Windows for a free loopback port. The server must start successfully before use.
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start()
$settings.P4PORT = "127.0.0.1:$($listener.LocalEndpoint.Port)"
$listener.Stop()
$process = $null
function Invoke-TestP4 {
    $result = @($input | & p4 @args)
    if ($LASTEXITCODE -ne 0) { throw "p4 $args failed ($LASTEXITCODE)" }
    $result
}
try {
    foreach ($name in $settings.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $settings[$name], 'Process')
    }
    & $P4d -r $server -xi | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not initialize the Unicode test server.' }
    $process = Start-Process -FilePath $P4d -ArgumentList @('-r', "`"$server`"", '-p', $settings.P4PORT, '-L', "`"$(Join-Path $root 'server.log')`"") -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 1
    if ($process.HasExited) { throw 'The disposable p4d did not start.' }
    @"
Client: eol-test
Owner: eol-test
Root: $workspace
LineEnd: unix
View:
    //depot/... //eol-test/...
"@ | Invoke-TestP4 client -i | Out-Null
    $utf8 = [Text.UTF8Encoding]::new($false)
    # Use a Unix-rendered client on Windows: this client version already tolerates LF in
    # a win/local client, but the inverse mismatch reliably exercises the same defect.
    $large = ((1..12000 | ForEach-Object { "unchanged line $_ padding to exceed the capture budget" }) -join "`n") + "`n"
    [IO.File]::WriteAllText((Join-Path $workspace 'a-eol-only.txt'), $large, $utf8)
    [IO.File]::WriteAllText((Join-Path $workspace 'b-mixed.txt'), $large + "semantic-before`n", $utf8)
    [IO.File]::WriteAllText((Join-Path $workspace 'c-whitespace.txt'), "space change`ntab change`n", $utf8)
    Invoke-TestP4 add -t text "$workspace\..." | Out-Null
    Invoke-TestP4 submit -d 'Synthetic baseline' | Out-Null
    $created = @"
Change: new
Client: eol-test
User: eol-test
Description:
    Synthetic line-ending regression
"@ | Invoke-TestP4 change -i
    if (($created -join '') -notmatch 'Change (\d+) created') { throw 'No pending changelist was created.' }
    $env:CROSS_REVIEW_P4_TEST_CL = $Matches[1]
    Invoke-TestP4 edit -c $env:CROSS_REVIEW_P4_TEST_CL "$workspace\..." | Out-Null
    [IO.File]::WriteAllText((Join-Path $workspace 'a-eol-only.txt'), $large.Replace("`n", "`r`n"), $utf8)
    [IO.File]::WriteAllText((Join-Path $workspace 'b-mixed.txt'), $large.Replace("`n", "`r`n") + "semantic-after`r`n", $utf8)
    [IO.File]::WriteAllText((Join-Path $workspace 'c-whitespace.txt'), "space  change`r`ntab`tchange`r`n", $utf8)

    $raw = (Invoke-TestP4 diff -du "$workspace\a-eol-only.txt") -join "`n"
    if ($raw.Length -lt 400000) { throw "Fixture did not reproduce the whole-file diff ($($raw.Length) bytes): $($raw.Substring(0, [Math]::Min(500, $raw.Length)))" }
    Write-Host "Unfiltered EOL-only diff: $($raw.Length) characters (over the 400 kB cap)."
    # Exercise the actual capture, including its shared budget and no-op listing, rather
    # than merely proving that a hand-written p4 command supports the desired flags.
    Push-Location $PSScriptRoot
    try {
        $ErrorActionPreference = 'Continue' # cargo's ordinary progress is on stderr
        $output = (& cargo test live_capture_against_a_real_changelist -- --nocapture 2>&1 | Out-String)
        $testExit = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
    } finally { Pop-Location }
    if ($testExit -ne 0) { throw "Capture test failed:`n$output" }
    foreach ($expected in @(
        '(no textual changes vs the depot, ignoring line endings: //depot/a-eol-only.txt)',
        '+semantic-after', '+space  change', "+tab`tchange"
    )) {
        if (-not $output.Contains($expected)) { throw "Capture lost expected evidence: $expected" }
    }
    if ($output.Contains('live warning:')) { throw "Capture was incomplete:`n$output" }
    Write-Host 'PASS: EOL-only changes use no diff budget; mixed content, spaces and tabs survive capture.'
} finally {
    if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id; $process.WaitForExit() }
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
        [IO.Path]::GetFileName($resolved).StartsWith('cross-review-eol-')) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
