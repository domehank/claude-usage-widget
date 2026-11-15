# Claude Code status line for the Claude usage taskbar widget.
#
# Claude Code runs this script after each reply and passes session info as JSON on stdin.
# The script does two things:
#   1. saves your 5-hour and weekly usage to %USERPROFILE%\.claude\usage.json (the widget reads it)
#   2. prints a short status line for Claude Code: model name and current folder
#
# Setup: see the README in the folder above this one.

[Console]::OutputEncoding = [Text.Encoding]::UTF8
$json = [Console]::In.ReadToEnd() | ConvertFrom-Json

# 1. Save the usage numbers. Written to a temp file and then moved into place, so the widget
#    never reads a half-written file. rate_limits is only present for Claude Pro/Max plans.
$rl = $json.rate_limits
if ($rl) {
    $path = Join-Path $env:USERPROFILE '.claude\usage.json'
    $tmp = "$path.$PID.tmp"
    try {
        $obj = [ordered]@{
            five_hour = $rl.five_hour
            seven_day = $rl.seven_day
            updated   = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
        }
        [IO.File]::WriteAllText($tmp, ($obj | ConvertTo-Json -Compress -Depth 4))
        Move-Item -Force $tmp $path
    }
    catch { Remove-Item $tmp -ErrorAction SilentlyContinue }
}

# 2. Print the status line. Change this part freely; the widget only needs step 1.
$model = $json.model.display_name
$dir = $json.workspace.current_dir
if (-not $dir) { $dir = $json.cwd }
$folder = if ($dir) { Split-Path $dir -Leaf } else { '' }
[Console]::Out.Write((@($model, $folder) | Where-Object { $_ }) -join ' | ')
