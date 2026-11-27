//! Exercise the zero-argument launcher through a Windows pseudoconsole.
#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::{
    Console::{ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON},
    Pipes::CreatePipe,
    Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTUPINFOEXW,
    },
};

const TIMEOUT: Duration = Duration::from_secs(20);
const PROMPT: &str = r".\DataDuck";

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

fn pipe() -> io::Result<(File, File)> {
    let (mut read, mut write) = (null_mut(), null_mut());
    // Both handles are initialized on success and then transferred to File.
    unsafe {
        if CreatePipe(&mut read, &mut write, null(), 0) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((File::from_raw_handle(read), File::from_raw_handle(write)))
    }
}

struct Console(HPCON);

impl Drop for Console {
    fn drop(&mut self) {
        unsafe { ClosePseudoConsole(self.0) };
    }
}

struct Attributes(Vec<usize>);

impl Attributes {
    fn init() -> io::Result<Self> {
        let mut bytes = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        let mut storage = vec![0; bytes.div_ceil(size_of::<usize>())];
        // An array of machine words gives the opaque list suitable alignment.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut bytes)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(storage))
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast()) };
    }
}

struct Child(OwnedHandle);

impl Child {
    fn handle(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    fn wait(&self) -> u32 {
        assert_eq!(
            unsafe { WaitForSingleObject(self.handle(), TIMEOUT.as_millis() as u32) },
            WAIT_OBJECT_0,
            "DataDuck did not exit after the exit command"
        );
        let mut code = 0;
        assert_ne!(unsafe { GetExitCodeProcess(self.handle(), &mut code) }, 0);
        code
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // On a failed assertion, stop the process before closing its pseudoconsole.
        if unsafe { WaitForSingleObject(self.handle(), 0) } == WAIT_TIMEOUT {
            unsafe {
                TerminateProcess(self.handle(), 1);
                WaitForSingleObject(self.handle(), 2000);
            }
        }
    }
}

struct Session {
    // Drop order matters: first terminate the child, then close its pseudoconsole.
    child: Child,
    _console: Console,
    input: File,
    output: Receiver<Vec<u8>>,
    screen: Vec<u8>,
    cursor: usize,
}

impl Session {
    fn start(exe: &Path, cwd: &Path) -> io::Result<Self> {
        let (console_input, input) = pipe()?;
        let (mut output, console_output) = pipe()?;
        // ConPTY requires asynchronous draining of its output pipe. Start before spawning
        // so boot messages and cleanup can never block on the synchronous pipe buffer.
        let (send, receive) = channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 4096];
            while let Ok(count) = output.read(&mut bytes) {
                if count == 0 || send.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut console = 0;
        let result = unsafe {
            CreatePseudoConsole(
                COORD { X: 160, Y: 40 },
                console_input.as_raw_handle(),
                console_output.as_raw_handle(),
                0,
                &mut console,
            )
        };
        if result < 0 {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole: {result:#x}"
            )));
        }
        let console = Console(console);
        let mut attributes = Attributes::init()?;
        let list = attributes.0.as_mut_ptr().cast();
        let application = wide(exe.as_os_str());
        let directory = wide(cwd.as_os_str());
        let mut argument = OsString::from("\"");
        argument.push(exe.as_os_str());
        argument.push("\"");
        let mut arguments = wide(&argument);
        let mut info: STARTUPINFOEXW = unsafe { zeroed() };
        info.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        info.lpAttributeList = list;
        let mut process: PROCESS_INFORMATION = unsafe { zeroed() };
        unsafe {
            if UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console.0 as _,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            if CreateProcessW(
                application.as_ptr(),
                arguments.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                null(),
                directory.as_ptr(),
                &info.StartupInfo,
                &mut process,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            drop(OwnedHandle::from_raw_handle(process.hThread));
        }
        drop(console_input);
        drop(console_output);
        Ok(Self {
            child: Child(unsafe { OwnedHandle::from_raw_handle(process.hProcess) }),
            _console: console,
            input,
            output: receive,
            screen: Vec::new(),
            cursor: 0,
        })
    }

    fn enter(&mut self, line: &str) {
        self.input
            .write_all(format!("{line}\r").as_bytes())
            .unwrap();
    }

    fn expect(&mut self, text: &str) {
        let until = Instant::now() + TIMEOUT;
        loop {
            if let Some(found) = self.screen[self.cursor..]
                .windows(text.len())
                .position(|window| window == text.as_bytes())
            {
                self.cursor += found + text.len();
                return;
            }
            match self
                .output
                .recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                Ok(bytes) => self.screen.extend(bytes),
                Err(error) => panic!(
                    "ConPTY output ended waiting for {text:?} ({error})\n{}",
                    String::from_utf8_lossy(&self.screen)
                ),
            }
        }
    }
}

#[test]
fn opens_in_executable_folder_and_accepts_multiple_commands() {
    let base = tempfile::tempdir().unwrap();
    let project = base.path().join("project folder");
    let data = project.join("input files");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("sales data.csv"), "Model,Price\nDuck,1200\n").unwrap();
    let exe = project.join("DataDuck.exe");
    fs::copy(env!("CARGO_BIN_EXE_dataduck"), &exe).unwrap();

    let mut session = Session::start(&exe, base.path()).unwrap();
    session.expect("Folder:");
    session.expect(PROMPT);
    session.enter("invalid-command");
    session.expect("unrecognized subcommand");
    session.expect("For more information");
    session.expect(PROMPT);
    session.enter("help");
    session.expect("Usage:");
    session.expect("Print version");
    session.expect(PROMPT);
    session.enter(r#"inspect ".\input files\sales data.csv" --rows 2"#);
    session.expect(r#"1="Duck""#);
    session.expect(r#"2="1200""#);
    session.expect(PROMPT);
    session.enter(r#"build ".\input files\sales data.csv" -o "sales report.html""#);
    session.expect("1 accepted rows");
    session.expect(PROMPT);

    assert!(fs::read_to_string(project.join("sales report.html"))
        .unwrap()
        .contains(r#""values":["Duck","1200"]"#));
    assert!(!base.path().join("sales report.html").exists());
    session.enter("exit");
    assert_eq!(session.child.wait(), 0);
}

#[test]
fn no_arguments_with_redirected_io_still_print_usage_and_exit() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dataduck"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Usage:"), "{stderr}");
    assert!(output.stdout.is_empty());
}
