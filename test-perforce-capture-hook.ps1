<#
.SYNOPSIS
  End-to-end check of the capture hook (issue #174) on a disposable local Perforce server.
  Binary edits, an add, a delete and a move must come back as generated text in pending,
  submitted and shelved changelists, and an unchanged re-capture must be served from the cache
  without running the converter again. Requires p4, p4d and cargo. No model calls.
#>
[CmdletBinding()]
param([string]$P4d = 'C:\Program Files\Perforce\DVCS\p4d.exe')

$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ("cross-review-capture-hook-" + [guid]::NewGuid())
$server = Join-Path $root 'server'
$workspace = Join-Path $root 'workspace'
$hookDir = Join-Path $root 'hook'
New-Item -ItemType Directory -Path $server, $workspace, $hookDir | Out-Null
$saved = @{}
$settings = @{
    P4USER = 'hook-test'; P4CLIENT = 'hook-test'; P4CHARSET = 'utf8'; P4PASSWD = ''
    P4CONFIG = 'cross-review-no-config-' + [guid]::NewGuid()
    P4ENVIRO = (Join-Path $root 'p4enviro')
    P4TICKETS = (Join-Path $root 'p4tickets.txt')
    CROSS_REVIEW_P4_TEST_CWD = $workspace
    CROSS_REVIEW_P4_TEST_HOOK_CL = ''
    CROSS_REVIEW_P4_TEST_SHELVED = ''
}
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start()
$settings.P4PORT = "127.0.0.1:$($listener.LocalEndpoint.Port)"
$listener.Stop()
$process = $null
$savedInputEncoding = [Console]::InputEncoding
function Invoke-TestP4 {
    $result = @($input | & p4 @args)
    if ($LASTEXITCODE -ne 0) { throw "p4 $args failed ($LASTEXITCODE)" }
    $result
}

# The converter: each side's bytes as hex, one "TEXT <name> <hex>" line, so the expected
# generated text is predictable. It counts its runs, one line per run, in counter.txt.
$counter = Join-Path $hookDir 'counter.txt'
$script = Join-Path $hookDir 'convert.ps1'
@"
`$in = [Console]::In.ReadToEnd() | ConvertFrom-Json
Add-Content -LiteralPath '$counter' -Value 'run'
`$files = @()
foreach (`$f in `$in.files) {
    `$rec = @{ id = `$f.id; path = (`$f.path + '.txt') }
    foreach (`$side in 'base', 'modified') {
        `$s = `$f.`$side
        if (`$null -eq `$s) { continue }
        if (-not `$s.file) { continue }
        `$hex = ([IO.File]::ReadAllBytes(`$s.file) | ForEach-Object { `$_.ToString('x2') }) -join ''
        `$out = Join-Path `$in.temp_dir ("`$(`$f.id)-`$side.txt")
        [IO.File]::WriteAllText(`$out, "TEXT `$side `$hex``n")
        `$rec["`${side}_text_file"] = `$out
    }
    `$files += `$rec
}
@{ files = `$files; note = 'hex dump of each side' } | ConvertTo-Json -Depth 5 -Compress
"@ | Set-Content -LiteralPath $script -Encoding UTF8

function Invoke-Capture([string]$Change, [string]$Expect, [switch]$Shelved) {
    Remove-Item -LiteralPath $counter -ErrorAction SilentlyContinue
    $env:CROSS_REVIEW_P4_TEST_HOOK_CL = $Change
    $env:CROSS_REVIEW_P4_TEST_HOOK_SCRIPT = $script
    $env:CROSS_REVIEW_P4_TEST_HOOK_EXPECT = $Expect
    $env:CROSS_REVIEW_P4_TEST_HOOK_COUNTER = $counter
    $env:CROSS_REVIEW_P4_TEST_SHELVED = if ($Shelved) { '1' } else { '' }
    Push-Location $PSScriptRoot
    try {
        $ErrorActionPreference = 'Continue'
        $output = (& cargo test live_capture_hook_converts_binaries -- --nocapture 2>&1 | Out-String)
        $exit = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
    } finally { Pop-Location }
    if ($exit -ne 0) { throw "Capture-hook test failed for change $Change`n$output" }
    if (-not $output.Contains('live_capture_hook_converts_binaries ... ok')) { throw "Test did not run:`n$output" }
    if ($output.Contains('skipping live capture-hook test')) { throw "Test skipped:`n$output" }
    Write-Host "PASS: change $Change"
}

try {
    [Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
    foreach ($name in $settings.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $settings[$name], 'Process')
    }
    foreach ($name in 'CROSS_REVIEW_P4_TEST_HOOK_SCRIPT', 'CROSS_REVIEW_P4_TEST_HOOK_EXPECT', 'CROSS_REVIEW_P4_TEST_HOOK_COUNTER') {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    & $P4d -r $server -xi | Out-Null
    foreach ($configurable in 'dm.user.noautocreate=0', 'dm.user.resetpassword=0', 'dm.user.setinitialpasswd=1') {
        & $P4d -r $server "-cset $configurable" | Out-Null
    }
    $process = Start-Process -FilePath $P4d -ArgumentList @('-r', "`"$server`"", '-p', $settings.P4PORT) -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 1
    if ($process.HasExited) { throw 'The disposable p4d did not start.' }
    $password = 'Hook-' + [guid]::NewGuid().ToString('N').Substring(0, 12) + '-7a'
    "$password`n$password" | Invoke-TestP4 passwd | Out-Null
    $password | Invoke-TestP4 login | Out-Null
    "Client: hook-test`nOwner: hook-test`nRoot: $workspace`nView:`n    //depot/... //hook-test/...`n" | Invoke-TestP4 client -i | Out-Null

    $bytes = { param($name, [byte[]]$b) [IO.File]::WriteAllBytes((Join-Path $workspace $name), $b) }
    & $bytes 'edit.uasset' ([byte[]](1, 0, 2))
    & $bytes 'gone.uasset' ([byte[]](7, 0, 7))
    & $bytes 'old.uasset' ([byte[]](9, 0, 9))
    & $bytes 'skip.bin' ([byte[]](3, 0, 3))
    Invoke-TestP4 add -t binary "$workspace\..." | Out-Null
    Invoke-TestP4 submit -d 'Synthetic baseline' | Out-Null

    # Pending: an edit, a delete, an add and a move, plus a binary the glob does not match.
    $created = "Change: new`nClient: hook-test`nUser: hook-test`nDescription:`n    Binary change`n" | Invoke-TestP4 change -i
    if (($created -join '') -notmatch 'Change (\d+) created') { throw 'No pending changelist.' }
    $pending = $Matches[1]
    Invoke-TestP4 edit -c $pending "$workspace\edit.uasset" | Out-Null
    & $bytes 'edit.uasset' ([byte[]](1, 0, 2, 4))
    Invoke-TestP4 delete -c $pending "$workspace\gone.uasset" | Out-Null
    & $bytes 'new.uasset' ([byte[]](5, 0, 5))
    Invoke-TestP4 add -c $pending -t binary "$workspace\new.uasset" | Out-Null
    Invoke-TestP4 edit -c $pending "$workspace\old.uasset" | Out-Null
    Invoke-TestP4 move -c $pending '//depot/old.uasset' '//depot/moved.uasset' | Out-Null
    Invoke-TestP4 edit -c $pending "$workspace\skip.bin" | Out-Null

    Invoke-Capture $pending ('-TEXT base 010002|+TEXT modified 01000204|+TEXT modified 050005|' +
        '-TEXT base 070007|# moved from //depot/old.uasset|Note from the capture hook: hex dump of each side')

    # Submitted: the same change, now historic. A submitted move has no movedFile, so it is the
    # add and the delete it is (verified on a real p4d).
    $submitted = ((Invoke-TestP4 submit -c $pending) -join "`n")
    if ($submitted -notmatch 'Change (\d+) submitted') { throw "Submit failed: $submitted" }
    $sub = $Matches[1]
    Invoke-Capture $sub '-TEXT base 010002|+TEXT modified 01000204|+TEXT modified 050005|-TEXT base 070007|+TEXT modified 090009|-TEXT base 090009'

    # Shelved: a binary edit on a shelf.
    $created = "Change: new`nClient: hook-test`nUser: hook-test`nDescription:`n    Shelf`n" | Invoke-TestP4 change -i
    if (($created -join '') -notmatch 'Change (\d+) created') { throw 'No shelf changelist.' }
    $shelf = $Matches[1]
    Invoke-TestP4 edit -c $shelf "$workspace\edit.uasset" | Out-Null
    & $bytes 'edit.uasset' ([byte[]](1, 0, 2, 4, 8))
    Invoke-TestP4 shelve -c $shelf | Out-Null
    Invoke-TestP4 revert -c $shelf "$workspace\..." | Out-Null
    Invoke-Capture $shelf '-TEXT base 01000204|+TEXT modified 0100020408' -Shelved

    Write-Host 'PASS: binary edit, add, delete and move convert in pending, submitted and shelved changes; a re-capture hits the cache.'
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
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
