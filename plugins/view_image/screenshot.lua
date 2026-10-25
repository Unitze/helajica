-- Captures the screen, or one window, to a PNG file with whatever the
-- platform ships: PowerShell on Windows, screencapture on macOS, and the
-- first of grim, import or gnome-screenshot found on Linux.

local M = {}

local CAPTURE_TIMEOUT_MS = 20000
local DBG_BRANCH = "START"
local SHOTS_DIR = "screenshots"
local PATH_VAR = "MAKI_SHOT_PATH"
local TITLE_VAR = "MAKI_SHOT_TITLE"

-- PrintWindow with PW_RENDERFULLCONTENT draws a window even when another
-- one covers it, GPU-rendered ones included. DPI awareness first, or the
-- rectangles come back scaled on a high-DPI display.
local WINDOWS_SCRIPT = [[
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Shot {
  [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
}
'@
[Shot]::SetProcessDPIAware() | Out-Null
$path = $env:MAKI_SHOT_PATH
New-Item -ItemType Directory -Force -Path (Split-Path $path) | Out-Null
$title = $env:MAKI_SHOT_TITLE
if ($title) {
  $proc = Get-Process | Where-Object { $_.MainWindowHandle -ne 0 -and ($_.MainWindowTitle -like "*$title*" -or $_.ProcessName -like "*$title*") } | Select-Object -First 1
  if (-not $proc) { throw "no window or program named like '$title'" }
  $rect = New-Object Shot+Rect
  [Shot]::GetWindowRect($proc.MainWindowHandle, [ref]$rect) | Out-Null
  $bitmap = New-Object System.Drawing.Bitmap ($rect.Right - $rect.Left), ($rect.Bottom - $rect.Top)
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  $hdc = $graphics.GetHdc()
  [Shot]::PrintWindow($proc.MainWindowHandle, $hdc, 2) | Out-Null
  $graphics.ReleaseHdc($hdc)
} else {
  $screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $bitmap = New-Object System.Drawing.Bitmap $screen.Width, $screen.Height
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  $graphics.CopyFromScreen($screen.Left, $screen.Top, 0, 0, $bitmap.Size)
}
$bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
]]

local function windows_command()
  return {
    "powershell",
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "Add-Type -AssemblyName System.Windows.Forms; " .. WINDOWS_SCRIPT,
  }
end

-- `window` is honoured on Windows only; elsewhere the whole screen is taken.
local function unix_command(path)
  if maki.fn.executable("screencapture") == 1 then
    return { "screencapture", "-x", path }
  end
  if maki.fn.executable("grim") == 1 then
    return { "grim", path }
  end
  if maki.fn.executable("import") == 1 then
    return { "import", "-window", "root", path }
  end
  if maki.fn.executable("gnome-screenshot") == 1 then
    return { "gnome-screenshot", "-f", path }
  end
  return nil
end

-- Windows PowerShell ships with every Windows and nowhere else; Linux and
-- macOS installs are named `pwsh`.
local function is_windows()
  return maki.fn.executable("powershell") == 1
end

local taken = 0

-- A fresh path per shot under the state dir, so earlier shots stay around to
-- compare with and to show the user.
local function next_path()
  local base = maki.env.state_dir() or "."
  taken = taken + 1
  return maki.fs.joinpath(base, SHOTS_DIR, string.format("shot-%d-%d.png", os.time(), taken))
end

local function make_dirs(path)
  local dir = maki.fs.dirname(path)
  local made, mkdir_err = maki.fs.mkdir(dir, { parents = true })
  if not made then
    return nil, "cannot create " .. dir .. ": " .. tostring(mkdir_err)
  end
  return true
end

-- The PNG's path, or nil and why it could not be taken.
function M.capture(window)
  local path = next_path()
  maki.log.error("DBG branch=" .. DBG_BRANCH .. " exe=" .. tostring(maki.fn.executable("powershell")) .. " state=" .. tostring(maki.env.state_dir()))
  local command
  if is_windows() then
    command = windows_command()
  else
    command = unix_command(path)
    if not command then
      return nil, "no screenshot tool found: install grim, ImageMagick (import) or gnome-screenshot"
    end
  end
  local made, mkdir_err = make_dirs(path)
  if not made then
    return nil, mkdir_err
  end
  local job, start_err = maki.fn.jobstart(command, { env = { [PATH_VAR] = path, [TITLE_VAR] = window or "" } })
  if not job then
    return nil, "cannot start the capture: " .. tostring(start_err)
  end
  local result, wait_err = maki.fn.jobwait(job, CAPTURE_TIMEOUT_MS)
  if not result then
    maki.fn.jobstop(job)
    return nil, "capture did not finish: " .. tostring(wait_err)
  end
  if result.exit_code ~= 0 then
    local detail = (result.stderr ~= "" and result.stderr) or result.stdout
    return nil, "capture failed: " .. detail
  end
  return path
end

return M
