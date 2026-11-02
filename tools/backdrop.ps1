# A full-width, top-anchored backdrop window for visual checks of the line.
#
# It sits above the desktop but below Pegline (start this first, then the app),
# so screenshots of the strip show the cards against a known colour instead of
# whatever the desktop happens to be showing. Close it with:
#   Get-Process pegline-backdrop | Stop-Process
#
#   pwsh -File tools\backdrop.ps1 -Top '#d8d2c8' -Bottom '#b9b1a4' -Height 1100
#   pwsh -File tools\backdrop.ps1 -Top '#1a1d22' -Bottom '#0b0d10' -Height 1100

param(
    [string]$Top = '#5b6470',
    [string]$Bottom = '#2a3038',
    [int]$Height = 1100,
    [switch]$Grid
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$topColor = [System.Drawing.ColorTranslator]::FromHtml($Top)
$bottomColor = [System.Drawing.ColorTranslator]::FromHtml($Bottom)
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds

$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::None
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
$form.Bounds = New-Object System.Drawing.Rectangle 0, 0, $bounds.Width, $Height
$form.TopMost = $true
$form.ShowInTaskbar = $false
$form.Text = 'pegline-backdrop'
$form.BackColor = $topColor

$form.add_Paint({
    param($sender, $event)
    $g = $event.Graphics
    $rect = New-Object System.Drawing.Rectangle 0, 0, $sender.Width, $sender.Height
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        $rect, $script:topColor, $script:bottomColor, [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
    $g.FillRectangle($brush, $rect)
    if ($script:Grid) {
        $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(26, 255, 255, 255)), 2
        for ($x = 0; $x -lt $sender.Width; $x += 120) {
            $g.DrawLine($pen, $x, 0, $x, $sender.Height)
        }
        for ($y = 0; $y -lt $sender.Height; $y += 120) {
            $g.DrawLine($pen, 0, $y, $sender.Width, $y)
        }
        $pen.Dispose()
    }
    $brush.Dispose()
})

# Let a harness wait for the window before it starts Pegline.
$script:ready = Join-Path $env:TEMP 'pegline-backdrop.ready'
Set-Content -LiteralPath $script:ready -Value $PID
$form.add_FormClosed({ Remove-Item -LiteralPath $script:ready -ErrorAction SilentlyContinue })

[System.Windows.Forms.Application]::Run($form)
