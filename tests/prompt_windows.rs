//! Exercise the zero-argument launcher in a fresh Windows console.
#![cfg(windows)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::mem::zeroed;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::{
    Console::{
        GetConsoleScreenBufferInfo, ReadConsoleOutputCharacterW, WriteConsoleInputW,
        CONSOLE_SCREEN_BUFFER_INFO, COORD, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT,
        KEY_EVENT_RECORD, KEY_EVENT_RECORD_0,
    },
    Threading::CREATE_NEW_CONSOLE,
};

const TIMEOUT: Duration = Duration::from_secs(20);
const PROMPT: &str = r".\DataDuck";
const HELPER: &str = "run_prompt_in_owned_console";
const HELPER_ROOT: &str = "DATADUCK_TEST_CONSOLE_ROOT";

struct Process(Child);

impl Process {
    fn wait(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let end = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= end {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "console process timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct Session {
    child: Process,
    input: File,
    output: File,
}

impl Session {
    fn start(exe: &Path, cwd: &Path) -> io::Result<Self> {
        // The CI runner redirects its standard streams. Pass explicit console handles
        // to DataDuck so the child sees the same streams it gets from Explorer.
        let input = OpenOptions::new().read(true).write(true).open("CONIN$")?;
        let output = OpenOptions::new().read(true).write(true).open("CONOUT$")?;
        let child = Command::new(exe)
            .current_dir(cwd)
            .stdin(input.try_clone()?)
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?)
            .spawn()?;
        Ok(Self {
            child: Process(child),
            input,
            output,
        })
    }

    fn enter(&self, line: &str) -> io::Result<()> {
        let records: Vec<_> = line
            .encode_utf16()
            .chain([u16::from(b'\r')])
            .flat_map(|character| {
                [1, 0].map(|down| INPUT_RECORD {
                    EventType: KEY_EVENT as u16,
                    Event: INPUT_RECORD_0 {
                        KeyEvent: KEY_EVENT_RECORD {
                            bKeyDown: down,
                            wRepeatCount: 1,
                            wVirtualKeyCode: if character == 13 { 13 } else { 0xe7 },
                            wVirtualScanCode: 0,
                            uChar: KEY_EVENT_RECORD_0 {
                                UnicodeChar: character,
                            },
                            dwControlKeyState: 0,
                        },
                    },
                })
            })
            .collect();
        let mut written = 0;
        // Windows copies the supplied key records into the console queue.
        if unsafe {
            WriteConsoleInputW(
                self.input.as_raw_handle(),
                records.as_ptr(),
                records.len() as u32,
                &mut written,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        assert_eq!(written as usize, records.len());
        Ok(())
    }

    fn screen(&self) -> io::Result<(String, String)> {
        // Capture the text cells through the current cursor line for progress checks.
        // Unlike the helper's redirected stderr, this buffer contains the real prompt.
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = zeroed();
            let handle = self.output.as_raw_handle();
            if GetConsoleScreenBufferInfo(handle, &mut info) == 0 {
                return Err(io::Error::last_os_error());
            }
            let cells = info.dwSize.X as usize * (info.dwCursorPosition.Y as usize + 1);
            let mut text = vec![0; cells];
            let mut read = 0;
            if ReadConsoleOutputCharacterW(
                handle,
                text.as_mut_ptr(),
                cells as u32,
                COORD { X: 0, Y: 0 },
                &mut read,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            text.truncate(read as usize);
            let row = info.dwCursorPosition.Y as usize * info.dwSize.X as usize;
            let current_line = String::from_utf16_lossy(text.get(row..).unwrap_or_default());
            Ok((String::from_utf16_lossy(&text), current_line))
        }
    }

    fn expect(&mut self, text: &str) {
        let until = Instant::now() + TIMEOUT;
        loop {
            // Earlier lines can scroll out of the buffer as each command prints.
            // A blank command line confirms the prompt has returned after its output.
            let (screen, current_line) = self.screen().unwrap();
            let visible = if text == PROMPT {
                current_line.trim_end() == PROMPT
            } else {
                screen.contains(text)
            };
            if visible {
                return;
            }
            let status = self.child.0.try_wait().unwrap();
            assert!(
                status.is_none() && Instant::now() < until,
                "Console did not show {text:?} (exit {status:?})\n{}",
                screen.trim_end(),
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[test]
fn opens_in_executable_folder_and_accepts_multiple_commands() {
    let base = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HELPER, "--ignored", "--nocapture"])
        .env(HELPER_ROOT, base.path())
        .creation_flags(CREATE_NEW_CONSOLE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut helper = Process(output);
    let status = helper.wait(Duration::from_secs(60)).unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    helper
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    helper
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "{stdout}\n{stderr}"
    );
    assert!(base
        .path()
        .join("project folder/sales report.html")
        .is_file());
}

#[test]
#[ignore = "launched by the console test in a separate Windows console"]
fn run_prompt_in_owned_console() {
    let Some(base) = std::env::var_os(HELPER_ROOT) else {
        return;
    };
    let base = Path::new(&base);
    let project = base.join("project folder");
    let data = project.join("input files");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("sales data.csv"), "Model,Price\nDuck,1200\n").unwrap();
    let exe = project.join("DataDuck.exe");
    fs::copy(env!("CARGO_BIN_EXE_dataduck"), &exe).unwrap();

    let mut session = Session::start(&exe, base).unwrap();
    session.expect("Folder:");
    session.expect(PROMPT);
    session.enter("invalid-command").unwrap();
    session.expect("unrecognized subcommand");
    session.expect("For more information");
    session.expect(PROMPT);
    session.enter("help").unwrap();
    session.expect("Usage:");
    session.expect("Print version");
    session.expect(PROMPT);
    session
        .enter(r#"inspect ".\input files\sales data.csv" --rows 2"#)
        .unwrap();
    session.expect(r#"1="Duck""#);
    session.expect(r#"2="1200""#);
    session.expect(PROMPT);
    session
        .enter(r#"build ".\input files\sales data.csv" -o "sales report.html""#)
        .unwrap();
    session.expect("1 accepted rows");
    session.expect(PROMPT);

    assert!(fs::read_to_string(project.join("sales report.html"))
        .unwrap()
        .contains(r#""values":["Duck","1200"]"#));
    assert!(!base.join("sales report.html").exists());
    session.enter("exit").unwrap();
    assert_eq!(session.child.wait(TIMEOUT).unwrap().code(), Some(0));
}

#[test]
fn no_arguments_with_redirected_io_still_print_usage_and_exit() {
    let output = Command::new(env!("CARGO_BIN_EXE_dataduck"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Usage:"), "{stderr}");
    assert!(output.stdout.is_empty());
}
