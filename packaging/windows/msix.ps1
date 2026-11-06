# Build dist\mp4-splitter-<version>-x64.msix for the Microsoft Store (unsigned; the
# Store signs it). As Rustshot's packaging\windows\msix.ps1 (Apache-2.0).
#
# The identity comes from Partner Center (Product identity), through the environment:
#   MSIX_IDENTITY_NAME, MSIX_PUBLISHER (CN=...), MSIX_PUBLISHER_DISPLAY
# Without them it builds with the placeholders below and says so in the file name
# (...-x64-PLACEHOLDER.msix): good for testing, refused by the Store.
#
# The exe: MSIX_EXE, or <CARGO_TARGET_DIR or target>\release\mp4-splitter.exe.
# MSIX_DIST (default dist) is where the package and its staging folder go.
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path (Join-Path $PSScriptRoot '..\..'))

$v = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
if ($v -notmatch '^\d+\.\d+\.\d+$') { throw "MSIX needs a plain x.y.z version, got '$v'" }

# PLACEHOLDERS: not real values. Docs\STORES.md lists what replaces them.
$placeholderName = 'PLACEHOLDER.MP4Splitter'
$placeholderPublisher = 'CN=PLACEHOLDER-FROM-PARTNER-CENTER'
$placeholderDisplay = 'PLACEHOLDER Publisher'

$set = @($env:MSIX_IDENTITY_NAME, $env:MSIX_PUBLISHER, $env:MSIX_PUBLISHER_DISPLAY) | Where-Object { $_ }
if ($set.Count -notin 0, 3) { throw 'Set all three of MSIX_IDENTITY_NAME, MSIX_PUBLISHER and MSIX_PUBLISHER_DISPLAY, or none' }
$real = $set.Count -eq 3
$name = if ($real) { $env:MSIX_IDENTITY_NAME } else { $placeholderName }
$pub = if ($real) { $env:MSIX_PUBLISHER } else { $placeholderPublisher }
$disp = if ($real) { $env:MSIX_PUBLISHER_DISPLAY } else { $placeholderDisplay }
if (-not $real) { Write-Warning 'No Partner Center identity set: building with PLACEHOLDER values. The Store will refuse this package.' }
if ($pub -notmatch '^CN=') { throw "MSIX_PUBLISHER must start with CN=, got '$pub'" }

# Drop outputs from earlier runs so CI never uploads a stale package.
$dist = if ($env:MSIX_DIST) { $env:MSIX_DIST } else { 'dist' }
New-Item -ItemType Directory -Force $dist | Out-Null
Get-ChildItem $dist -Filter 'mp4-splitter-*.msix' | Remove-Item -Force
$out = Join-Path $dist ("mp4-splitter-$v-x64" + $(if ($real) { '' } else { '-PLACEHOLDER' }) + '.msix')
$exe = if ($env:MSIX_EXE) { $env:MSIX_EXE } else { Join-Path $(if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }) 'release\mp4-splitter.exe' }
if (-not (Test-Path $exe)) { throw "$exe missing (cargo build --release first)" }

$esc = { param($s) [System.Security.SecurityElement]::Escape($s) }
$stage = Join-Path $dist 'stage-msix'
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force "$stage\Assets" | Out-Null
Copy-Item $exe "$stage\mp4-splitter.exe"
Copy-Item LICENSE "$stage\LICENSE.txt"
Copy-Item NOTICE "$stage\NOTICE.txt"
Copy-Item packaging\icons\msix\*.png "$stage\Assets"

# The package's languages (what the Store lists): one per lang\<tag>.txt, in the
# forms Microsoft's "supported languages" table lists for the Store. en-us comes
# first: the first <Resource> is the default language. A new lang file without an
# entry here stops the build, so its Store code is picked on purpose. No
# resources.pri: a full-trust package declares its languages in the manifest only.
$storeLang = @{
    'en' = 'en-us'; 'ar' = 'ar'; 'bg' = 'bg'; 'cs' = 'cs'; 'da' = 'da'; 'de' = 'de'
    'el' = 'el'; 'es' = 'es'; 'fi' = 'fi'; 'fr' = 'fr'; 'he' = 'he'; 'hi' = 'hi'
    'hr' = 'hr'; 'hu' = 'hu'; 'id' = 'id'; 'it' = 'it'; 'ja' = 'ja'; 'ko' = 'ko'
    'lt' = 'lt'; 'nb' = 'nb'; 'nl' = 'nl'; 'pl' = 'pl'; 'pt-BR' = 'pt-br'; 'pt-PT' = 'pt-pt'
    'ro' = 'ro'; 'ru' = 'ru'; 'sl' = 'sl'; 'sr-Latn' = 'sr-Latn'; 'sv' = 'sv'; 'th' = 'th'
    'tr' = 'tr'; 'uk' = 'uk'; 'vi' = 'vi'; 'zh-Hans' = 'zh-Hans'; 'zh-Hant' = 'zh-Hant'
}
$tags = @(Get-ChildItem lang -Filter *.txt | ForEach-Object BaseName | Sort-Object { $_ -ne 'en' }, { $_ })
if ($tags[0] -cne 'en') { throw 'lang\en.txt missing' }
$langs = foreach ($t in $tags) {
    # Hashtable keys ignore case; the file name's spelling must match too.
    if (@($storeLang.Keys) -cnotcontains $t) { throw "lang\$t.txt has no Store language code in msix.ps1 (`$storeLang)" }
    $storeLang[$t]
}
$resources = ($langs | ForEach-Object { "    <Resource Language=`"$_`" />" }) -join "`n"
Write-Host "languages ($($langs.Count)): $($langs -join ' ')"

$manifest = (Get-Content packaging\windows\AppxManifest.xml.in -Raw).
    Replace('@IDENTITY_NAME@', (& $esc $name)).
    Replace('@PUBLISHER@', (& $esc $pub)).
    Replace('@PUBLISHER_DISPLAY@', (& $esc $disp)).
    Replace('@VERSION@', "$v.0").
    Replace('@RESOURCES@', $resources)
Set-Content -Path "$stage\AppxManifest.xml" -Value $manifest -Encoding utf8

$bin = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\10.*\x64\makeappx.exe" -ErrorAction SilentlyContinue |
    Sort-Object { [version]$_.Directory.Parent.Name } -Descending | Select-Object -First 1
if (-not $bin) { throw 'makeappx.exe not found (install the Windows 10/11 SDK)' }
& $bin.FullName pack /o /d $stage /p $out
if ($LASTEXITCODE) { throw "makeappx failed ($LASTEXITCODE)" }
Write-Host "wrote $out"
