<#
.SYNOPSIS
Creates Windows filesystem fixtures for sb-probe (milestone 1).

.DESCRIPTION
Builds ordinary, empty, MFT-resident, sparse, compressed, hard-linked,
alternate-data-stream, hidden/system, junction, and symlink cases under the
given directory, which must be on NTFS. Symlink creation needs Developer Mode
or elevation; failures are reported and the remaining fixtures are still made.

.EXAMPLE
pwsh scripts/probe-fixtures.ps1 $env:TEMP\sb-fixtures
#>
param([Parameter(Mandatory)][string]$Root)

$ErrorActionPreference = 'Stop'
if (Test-Path $Root) { throw "Refusing to reuse existing path: $Root" }
New-Item -ItemType Directory $Root | Out-Null
$Root = (Resolve-Path $Root).Path

function Write-Random([string]$Path, [int]$Bytes) {
    $data = [byte[]]::new($Bytes)
    [Random]::new(42).NextBytes($data)
    [IO.File]::WriteAllBytes($Path, $data)
}

$results = [ordered]@{}
function Try-Fixture([string]$Name, [scriptblock]$Make) {
    try { & $Make; $results[$Name] = 'ok' }
    catch { $results[$Name] = "failed: $($_.Exception.Message)" }
}

Try-Fixture 'empty.txt' { New-Item -ItemType File "$Root\empty.txt" | Out-Null }
Try-Fixture 'tiny.txt (MFT-resident)' { [IO.File]::WriteAllText("$Root\tiny.txt", 'x' * 100) }
Try-Fixture 'ordinary.bin (1 MiB + 1)' { Write-Random "$Root\ordinary.bin" (1MB + 1) }

Try-Fixture 'compressed.txt (16 MiB text, NTFS compression)' {
    $line = 'SpaceBadger compressible fixture line 0123456789 abcdefghijklmnopqrstuvwxyz' + "`n"
    $sb = [Text.StringBuilder]::new()
    while ($sb.Length -lt 16MB) { [void]$sb.Append($line) }
    [IO.File]::WriteAllText("$Root\compressed.txt", $sb.ToString())
    compact.exe /c /q "$Root\compressed.txt" | Out-Null
    if ($LASTEXITCODE) { throw "compact exited $LASTEXITCODE" }
}

Try-Fixture 'sparse.bin (1 GiB length, 64 KiB data)' {
    Write-Random "$Root\sparse.bin" 64KB
    fsutil sparse setflag "$Root\sparse.bin" | Out-Null
    if ($LASTEXITCODE) { throw "fsutil sparse setflag exited $LASTEXITCODE" }
    fsutil file seteof "$Root\sparse.bin" 1073741824 | Out-Null
    if ($LASTEXITCODE) { throw "fsutil file seteof exited $LASTEXITCODE" }
}

Try-Fixture 'hardlink-a.bin + hardlink-b.bin (grown via a after linking)' {
    Write-Random "$Root\hardlink-a.bin" 256KB
    New-Item -ItemType HardLink -Path "$Root\hardlink-b.bin" -Target "$Root\hardlink-a.bin" | Out-Null
    # Grow through one name only: NTFS may leave the other name's directory entry stale.
    $s = [IO.File]::Open("$Root\hardlink-a.bin", 'Append')
    $s.Write([byte[]]::new(768KB), 0, 768KB); $s.Close()
}

Try-Fixture 'ads.txt (1 MiB alternate data stream)' {
    [IO.File]::WriteAllText("$Root\ads.txt", 'main stream')
    $data = [byte[]]::new(1MB); [Random]::new(7).NextBytes($data)
    Set-Content -Path "$Root\ads.txt" -Stream extra -Value $data -AsByteStream
}

Try-Fixture 'hidden-system.bin' {
    Write-Random "$Root\hidden-system.bin" 4096
    attrib.exe +h +s "$Root\hidden-system.bin"
}

Try-Fixture 'target\ (junction/symlink target with 2 MiB file)' {
    New-Item -ItemType Directory "$Root\target" | Out-Null
    Write-Random "$Root\target\inside.bin" 2MB
}
Try-Fixture 'junction -> target' {
    New-Item -ItemType Junction -Path "$Root\junction" -Target "$Root\target" | Out-Null
}
Try-Fixture 'dir-symlink -> target' {
    New-Item -ItemType SymbolicLink -Path "$Root\dir-symlink" -Target "$Root\target" | Out-Null
}
Try-Fixture 'file-symlink -> ordinary.bin' {
    New-Item -ItemType SymbolicLink -Path "$Root\file-symlink.bin" -Target "$Root\ordinary.bin" | Out-Null
}
Try-Fixture 'loop\ (junction to its own parent)' {
    New-Item -ItemType Directory "$Root\loop" | Out-Null
    New-Item -ItemType Junction -Path "$Root\loop\back" -Target "$Root\loop" | Out-Null
}

$results.GetEnumerator() | ForEach-Object { '{0,-60} {1}' -f $_.Key, $_.Value }
