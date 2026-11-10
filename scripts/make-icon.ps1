<#
.SYNOPSIS
  Regenerates assets\snapcap.ico (the SnapCap app icon): a red record dot in a white ring
  on a dark rounded tile at 16/24/32/48/64/128/256 px.
  Sizes below 256 use classic BMP entries (readable by every tool); 256 uses PNG.
.PARAMETER PreviewDir
  Optional folder to also write PNG previews of each size.
#>
param([string]$PreviewDir = "")
Add-Type -AssemblyName System.Drawing
$root = Split-Path $PSScriptRoot -Parent
$out = Join-Path $root "assets\snapcap.ico"
New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null

function New-IconBitmap([int]$S) {
    $bmp = New-Object System.Drawing.Bitmap $S, $S, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'; $g.PixelOffsetMode = 'HighQuality'; $g.Clear([System.Drawing.Color]::Transparent)

    # dark rounded tile
    $r = [single]($S * 0.22); $d = $r * 2; $w = [single]$S
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc(0, 0, $d, $d, 180, 90); $path.AddArc($w - $d, 0, $d, $d, 270, 90)
    $path.AddArc($w - $d, $w - $d, $d, $d, 0, 90); $path.AddArc(0, $w - $d, $d, $d, 90, 90); $path.CloseFigure()
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 23, 28, 38))), $path)

    $c = $S / 2.0
    # white ring
    $ringR = $S * 0.38; $ringW = [math]::Max(1.5, $S * 0.075)
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), $ringW
    $g.DrawEllipse($pen, [single]($c - $ringR), [single]($c - $ringR), [single]($ringR * 2), [single]($ringR * 2))
    # red dot with a dark outline
    $dotR = $S * 0.24; $ol = [math]::Max(1.0, $S * 0.05)
    $g.FillEllipse((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 10, 10, 12))),
        [single]($c - $dotR - $ol), [single]($c - $dotR - $ol), [single](($dotR + $ol) * 2), [single](($dotR + $ol) * 2))
    $g.FillEllipse((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 255, 38, 38))),
        [single]($c - $dotR), [single]($c - $dotR), [single]($dotR * 2), [single]($dotR * 2))
    $g.Dispose()
    return $bmp
}

function ConvertTo-PngBytes($bmp) { $ms = New-Object System.IO.MemoryStream; $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); return , $ms.ToArray() }

# classic icon image: BITMAPINFOHEADER (height doubled) + bottom-up BGRA pixels + 1bpp AND mask
function ConvertTo-DibBytes($bmp, [int]$S) {
    $maskRow = [int]([math]::Ceiling($S / 32.0) * 4)
    $ms = New-Object System.IO.MemoryStream; $bw = New-Object System.IO.BinaryWriter $ms
    $bw.Write([uint32]40); $bw.Write([int32]$S); $bw.Write([int32]($S * 2)); $bw.Write([uint16]1); $bw.Write([uint16]32)
    $bw.Write([uint32]0); $bw.Write([uint32]($S * $S * 4 + $maskRow * $S)); $bw.Write([int32]0); $bw.Write([int32]0); $bw.Write([uint32]0); $bw.Write([uint32]0)
    for ($y = $S - 1; $y -ge 0; $y--) {
        for ($x = 0; $x -lt $S; $x++) { $p = $bmp.GetPixel($x, $y); $bw.Write([byte]$p.B); $bw.Write([byte]$p.G); $bw.Write([byte]$p.R); $bw.Write([byte]$p.A) }
    }
    $bw.Write((New-Object byte[] ($maskRow * $S)))   # alpha channel carries transparency; mask all zero
    $bw.Flush(); return , $ms.ToArray()
}

$sizes = 16, 24, 32, 48, 64, 128, 256
$images = @()
foreach ($s in $sizes) {
    $bmp = New-IconBitmap $s
    if ($PreviewDir) { New-Item -ItemType Directory -Force $PreviewDir | Out-Null; $bmp.Save((Join-Path $PreviewDir "icon_$s.png")) }
    $images += , $(if ($s -ge 256) { ConvertTo-PngBytes $bmp } else { ConvertTo-DibBytes $bmp $s })
    $bmp.Dispose()
}

$fs = [System.IO.File]::Create($out); $bw = New-Object System.IO.BinaryWriter $fs
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)    # ICONDIR
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {                                          # ICONDIRENTRYs
    $s = $sizes[$i]; $len = $images[$i].Length; $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32); $bw.Write([uint32]$len); $bw.Write([uint32]$offset); $offset += $len
}
foreach ($img in $images) { $bw.Write($img) }
$bw.Close(); $fs.Close()
"wrote $out ($([math]::Round((Get-Item $out).Length / 1KB, 1)) KB, sizes: $($sizes -join ', '))"
