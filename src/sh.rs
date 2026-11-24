use std::process::Command;

/// A command that runs `cmd` through the platform shell.
pub fn command(cmd: &str) -> Command {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut c = Command::new(shell);
    c.args([flag, cmd]);
    c
}
