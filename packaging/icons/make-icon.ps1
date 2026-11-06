# Generates the app icon set from one drawing: an accent rounded square with
# white scissors (the header's mark, Lucide's scissors glyph on a 24-unit grid).
# Writes mp4-splitter-<size>.png and the multi-size mp4-splitter.ico next to
# this script. Run on Windows: pwsh -File packaging/icons/make-icon.ps1
Add-Type -AssemblyName System.Drawing.Common, System.Drawing.Primitives -ErrorAction SilentlyContinue
Add-Type -AssemblyName System.Drawing -ErrorAction SilentlyContinue

function New-Mark([int]$Size) {
    $bmp = [System.Drawing.Bitmap]::new($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)
    # The tile: the light theme's accent, a little lighter at the top left.
    $rad = [single]($Size * 0.22)
    $d = 2 * $rad
    $e = [single]$Size
    $path = [System.Drawing.Drawing2D.GraphicsPath]::new()
    $path.AddArc([single]0, [single]0, $d, $d, 180, 90)
    $path.AddArc($e - $d, [single]0, $d, $d, 270, 90)
    $path.AddArc($e - $d, $e - $d, $d, $d, 0, 90)
    $path.AddArc([single]0, $e - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    $brush = [System.Drawing.Drawing2D.LinearGradientBrush]::new(
        [System.Drawing.PointF]::new(0, 0), [System.Drawing.PointF]::new($e, $e),
        [System.Drawing.Color]::FromArgb(255, 0x76, 0x7D, 0xFF), [System.Drawing.Color]::FromArgb(255, 0x4A, 0x52, 0xE8))
    $g.FillPath($brush, $path)
    # The glyph: larger and bolder at small sizes, so it still reads at 16 px.
    $glyph = if ($Size -le 24) { 0.86 } elseif ($Size -le 48) { 0.76 } else { 0.68 }
    $width = if ($Size -le 24) { 2.5 } else { 2.2 }
    $k = [single]($Size * $glyph / 24)
    $o = [single](($Size - 24 * $k) / 2)
    $g.TranslateTransform($o, $o)
    $g.ScaleTransform($k, $k)
    $pen = [System.Drawing.Pen]::new([System.Drawing.Color]::White, [single]$width)
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $g.DrawEllipse($pen, [single]3, [single]3, [single]6, [single]6)
    $g.DrawEllipse($pen, [single]3, [single]15, [single]6, [single]6)
    $g.DrawLine($pen, [single]20, [single]4, [single]8.12, [single]15.88)
    $g.DrawLine($pen, [single]14.47, [single]14.48, [single]20, [single]20)
    $g.DrawLine($pen, [single]8.12, [single]8.12, [single]12, [single]12)
    $g.Dispose()
    $bmp
}

$dir = $PSScriptRoot
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$images = @()
foreach ($s in $sizes) {
    $b = New-Mark $s
    $png = Join-Path $dir "mp4-splitter-$s.png"
    $b.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
    $data = [System.IO.File]::ReadAllBytes($png)
    $images += , @($s, $data)
    $b.Dispose()
}
foreach ($s in 44, 50, 128, 150, 512, 1024) {
    $b = New-Mark $s
    $b.Save((Join-Path $dir "mp4-splitter-$s.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    $b.Dispose()
}

# The Store package's tiles and icons, from the same drawing.
$msix = Join-Path $dir 'msix'
New-Item -ItemType Directory -Force $msix | Out-Null
function Save-Mark([int]$Size, [string]$Name) {
    $b = New-Mark $Size
    $b.Save((Join-Path $msix $Name), [System.Drawing.Imaging.ImageFormat]::Png)
    $b.Dispose()
}
Save-Mark 44 'Square44x44Logo.png'
foreach ($s in 16, 24, 32, 48, 256) { Save-Mark $s "Square44x44Logo.targetsize-${s}_altform-unplated.png" }
Save-Mark 150 'Square150x150Logo.png'
Save-Mark 50 'StoreLogo.png'
# The wide tile: the mark in the middle of a clear canvas.
$wide = [System.Drawing.Bitmap]::new(310, 150, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($wide)
$g.Clear([System.Drawing.Color]::Transparent)
$b = New-Mark 150
$g.DrawImage($b, 80, 0, 150, 150)
$g.Dispose()
$b.Dispose()
$wide.Save((Join-Path $msix 'Wide310x150Logo.png'), [System.Drawing.Imaging.ImageFormat]::Png)
$wide.Dispose()

$ico = [System.IO.File]::Create((Join-Path $dir 'mp4-splitter.ico'))
$w = [System.IO.BinaryWriter]::new($ico)
$w.Write([int16]0); $w.Write([int16]1); $w.Write([int16]$images.Count)
$at = 6 + 16 * $images.Count
foreach ($i in $images) {
    $s = $i[0]; $data = [byte[]]$i[1]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([int16]1); $w.Write([int16]32); $w.Write([int]$data.Length); $w.Write([int]$at)
    $at += $data.Length
}
foreach ($i in $images) { $w.Write([byte[]]$i[1]) }
$w.Dispose()
Write-Host "wrote the icon set to $dir"
