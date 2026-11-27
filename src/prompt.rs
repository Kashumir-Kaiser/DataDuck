//! The Windows console opens this prompt when DataDuck starts without arguments.
use crate::resource::{MemoryGuard, CANCELLED};
use crate::{command_exit_code, install_cancel_handler, run_command, Cli};
use anyhow::{ensure, Context, Result};
use clap::Parser;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::os::windows::ffi::OsStringExt;
use std::sync::atomic::Ordering;
use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};

const PROMPT: &str = r".\DataDuck ";

pub fn should_open() -> bool {
    std::env::args_os().nth(1).is_none() && io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn arguments(line: &str) -> Result<Vec<OsString>> {
    ensure!(!line.contains('\0'), "command contains a NUL character");
    // Windows treats the first token as the executable. Supply it ourselves so
    // Windows-style quotes and backslashes work the same way for each argument.
    let wide: Vec<u16> = PROMPT
        .encode_utf16()
        .chain(line.encode_utf16())
        .chain([0])
        .collect();
    let mut count = 0;
    // CommandLineToArgvW returns one allocation of NUL-terminated strings.
    unsafe {
        let argv = CommandLineToArgvW(wide.as_ptr(), &mut count);
        if argv.is_null() {
            return Err(io::Error::last_os_error()).context("parse command");
        }
        let mut args = Vec::with_capacity(count as usize);
        for &arg in std::slice::from_raw_parts(argv, count as usize) {
            let mut len = 0;
            while *arg.add(len) != 0 {
                len += 1;
            }
            args.push(OsString::from_wide(std::slice::from_raw_parts(arg, len)));
        }
        LocalFree(argv.cast());
        Ok(args)
    }
}

pub fn run() -> Result<()> {
    let exe = std::env::current_exe().context("find the DataDuck executable")?;
    let folder = exe.parent().context("find the DataDuck folder")?;
    std::env::set_current_dir(folder).with_context(|| format!("open {}", folder.display()))?;
    let guard = MemoryGuard::install()?;
    install_cancel_handler()?;

    println!("DataDuck {}", env!("CARGO_PKG_VERSION"));
    println!("Folder: {}", folder.display());
    println!("Type help for commands or exit to close. Put paths with spaces in double quotes.");

    let mut line = String::new();
    loop {
        print!("{PROMPT}");
        io::stdout().flush()?;
        line.clear();
        match io::stdin().read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("read command"),
        }
        let input = line.trim();
        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            return Ok(());
        }
        if input.is_empty() {
            continue;
        }
        CANCELLED.store(false, Ordering::Relaxed);
        match arguments(input) {
            Ok(args) => match Cli::try_parse_from(args) {
                Ok(cli) => {
                    let _ = command_exit_code(run_command(cli, &guard));
                }
                Err(e) => e.print()?,
            },
            Err(e) => {
                let _ = command_exit_code(Err(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_uses_windows_path_quoting() {
        assert_eq!(
            arguments(r#"build "C:\Sales Reports\giá.xlsx" -o .\prices.html"#).unwrap(),
            [
                r".\DataDuck",
                "build",
                r"C:\Sales Reports\giá.xlsx",
                "-o",
                r".\prices.html"
            ]
            .map(OsString::from)
        );
    }
}
