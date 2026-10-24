//! Starting child processes from a desktop app.

use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A desktop app has no console for a console program to share, so Windows
/// opens a new one for every child, flashing it on screen. Children started
/// this way run without one, and so do theirs.
pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
