# Stress the capture pipeline: drop N fresh PNGs into the folder Pegline
# watches and sample its private memory while the cards land.
#
#   pwsh -File tools\stress.ps1 -Count 200
#   pwsh -File tools\stress.ps1 -Folder 'D:\shots' -DelayMs 60
#
# Pegline must be running (release build). The script creates files named
# `pegline-stress-*.png` and deletes them again at the end, so the real
# screenshots folder is left as it was. Cancel with Ctrl+C: the cleanup still
# runs via try/finally.

param(
    [int]$Count = 200,
    [string]$Folder = (Join-Path ([Environment]::GetFolderPath('MyPictures')) 'Screenshots'),
    [int]$DelayMs = 120,
    [int]$SettleSeconds = 45,
    [string]$ProcessName = 'pegline'
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

if (-not (Test-Path -LiteralPath $Folder)) {
    New-Item -ItemType Directory -Path $Folder -Force | Out-Null
}

$process = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $process) {
    throw "Pegline is not running. Start target\release\pegline.exe first."
}

function Get-PrivateMb {
    param($Process)
    $Process.Refresh()
    return [math]::Round($Process.PrivateMemorySize64 / 1MB, 1)
}

$baseline = Get-PrivateMb $process
$peak = $baseline
$created = [System.Collections.Generic.List[string]]::new()
$started = Get-Date
$lineCapacity = 12
$warmup = [Math]::Min($lineCapacity, $Count)

Write-Host ("stress: {0} captures -> {1} (empty line {2} MB private)" -f $Count, $Folder, $baseline)

try {
    for ($index = 1; $index -le $Count; $index++) {
        $width = 320
        $height = 200
        $bitmap = [System.Drawing.Bitmap]::new($width, $height)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        try {
            $hue = ($index * 37) % 360
            $color = [System.Drawing.Color]::FromArgb(255, (100 + $hue % 120), (80 + $hue % 160), (200 - $hue % 100))
            $graphics.Clear($color)
            $graphics.DrawString(
                ("stress {0:000}  {1:HH:mm:ss}" -f $index, (Get-Date)),
                [System.Drawing.SystemFonts]::DefaultFont,
                [System.Drawing.Brushes]::White,
                12,
                12)
        }
        finally {
            $graphics.Dispose()
        }

        $name = 'pegline-stress-{0:0000}.png' -f $index
        $path = Join-Path $Folder $name
        $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $bitmap.Dispose()
        $created.Add($path)

        Start-Sleep -Milliseconds $DelayMs
        if ($index -eq $warmup) {
            # The line is full now; this is the state the remaining captures must
            # not grow beyond (BEHAVIOR: 200 captures return to the baseline).
            Start-Sleep -Seconds 2
            $baseline = Get-PrivateMb $process
            Write-Host ("  warm-up done at {0} cards: baseline {1} MB" -f $index, $baseline)
        }
        if ($index % 20 -eq 0 -or $index -eq $Count) {
            $memory = Get-PrivateMb $process
            $peak = [Math]::Max($peak, $memory)
            Write-Host ("  {0,4}/{1}  private {2,6} MB" -f $index, $Count, $memory)
        }
    }

    # Let the last cards decode, the helper windows go idle and the line settle.
    # The app destroys its pooled flight/preview/ghost windows after 10-30 s, so
    # the settled number is sampled after that window has passed.
    for ($second = 2; $second -le $SettleSeconds; $second += 2) {
        Start-Sleep -Seconds 2
        $memory = Get-PrivateMb $process
        $peak = [Math]::Max($peak, $memory)
        if ($second % 10 -eq 0) {
            Write-Host ("  settle {0,3}/{1} s  private {2,6} MB" -f $second, $SettleSeconds, $memory)
        }
    }
    $settled = Get-PrivateMb $process
    $elapsed = ((Get-Date) - $started).TotalSeconds
    Write-Host ("stress: settled {0} MB, peak {1} MB, {2:N1} s" -f $settled, $peak, $elapsed)
    $limit = [math]::Round($baseline * 1.15 + 5, 1)
    if ($settled -le $limit) {
        Write-Host ("PASS: settled memory {0} MB <= {1} MB (baseline +15% +5 MB)" -f $settled, $limit)
    }
    else {
        Write-Host ("FAIL: settled memory {0} MB > {1} MB" -f $settled, $limit)
        exit 1
    }
}
finally {
    foreach ($path in $created) {
        Remove-Item -LiteralPath $path -ErrorAction SilentlyContinue
    }
    Write-Host ("stress: removed {0} generated files" -f $created.Count)
}
