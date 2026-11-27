//! Private run directory and quota accounting for live, seekable scratch files.
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tempfile::{NamedTempFile, TempDir};

pub const FREE_RESERVE: u64 = 2 << 30;
pub static CANCELLED: AtomicBool = AtomicBool::new(false);
pub fn check_cancel() -> io::Result<()> {
    if CANCELLED.load(Ordering::Relaxed) {
        Err(io::Error::other("run cancelled"))
    } else {
        Ok(())
    }
}
#[derive(Default)]
struct Usage {
    live: u64,
    peak: u64,
    until_check: u64,
    error: Option<String>,
}
struct Inner {
    // Close the exclusive owner file before removing its run directory on Windows.
    _owner: File,
    dir: TempDir,
    usage: Mutex<Usage>,
    limit: u64,
}
#[derive(Clone)]
pub struct Scratch(Arc<Inner>);
impl Scratch {
    pub fn new(parent: Option<&Path>, limit: u64) -> Result<Self> {
        let parent = parent.map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&parent)?;
        let dir = tempfile::Builder::new()
            .prefix("dataduck-")
            .tempdir_in(&parent)
            .with_context(|| format!("create scratch directory in {}", parent.display()))?;
        if fs2::available_space(dir.path())? < FREE_RESERVE {
            bail!("scratch volume needs 2 GiB free: {}", parent.display());
        }
        let owner = File::create(dir.path().join(".dataduck-run"))?;
        fs2::FileExt::try_lock_exclusive(&owner)?;
        Ok(Self(Arc::new(Inner {
            _owner: owner,
            dir,
            usage: Mutex::new(Usage::default()),
            limit,
        })))
    }
    pub fn path(&self) -> &Path {
        self.0.dir.path()
    }
    pub fn create(&self) -> io::Result<ScratchFile> {
        check_cancel()?;
        let file = NamedTempFile::new_in(self.path()).map_err(|e| self.fail(e))?;
        Ok(ScratchFile {
            file: Some(file),
            scratch: self.clone(),
            len: 0,
            pos: 0,
        })
    }
    fn fail(&self, e: io::Error) -> io::Error {
        self.0.usage.lock().unwrap().error =
            Some(format!("scratch {}: {e}", self.path().display()));
        e
    }
    pub fn check(&self) -> Result<()> {
        check_cancel()?;
        if let Some(e) = &self.0.usage.lock().unwrap().error {
            bail!("{e}");
        }
        Ok(())
    }
    pub fn peak(&self) -> u64 {
        self.0.usage.lock().unwrap().peak
    }
}
pub struct ScratchFile {
    file: Option<NamedTempFile>,
    scratch: Scratch,
    len: u64,
    pos: u64,
}
impl ScratchFile {
    pub fn path(&self) -> &Path {
        self.file.as_ref().unwrap().path()
    }
    pub fn park(mut self) -> io::Result<Parked> {
        self.flush()?;
        let path = self.file.take().unwrap().into_temp_path();
        Ok(Parked {
            path,
            len: std::mem::take(&mut self.len),
            scratch: self.scratch.clone(),
        })
    }
    pub fn reopen(&self) -> io::Result<File> {
        File::open(self.path())
    }
}
impl Write for ScratchFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        check_cancel()?;
        let end = self
            .pos
            .checked_add(b.len() as u64)
            .ok_or_else(|| io::Error::other("file too large"))?;
        let extra = end.saturating_sub(self.len);
        let mut u = self.scratch.0.usage.lock().unwrap();
        let total = u.live.saturating_add(extra);
        if total > self.scratch.0.limit {
            let msg=format!("scratch limit exceeded at {} (need at least {} live bytes, limit {}); increase --temp-limit-gib or change --temp-dir",self.scratch.path().display(),total,self.scratch.0.limit);
            u.error = Some(msg.clone());
            return Err(io::Error::other(msg));
        }
        if extra > 0 && extra >= u.until_check {
            let free = fs2::available_space(self.scratch.path())?;
            if free < FREE_RESERVE.saturating_add(extra) {
                let msg = format!(
                    "scratch volume {} needs 2 GiB free",
                    self.scratch.path().display()
                );
                u.error = Some(msg.clone());
                return Err(io::Error::other(msg));
            }
            u.until_check = 4 << 20;
        }
        match self.file.as_mut().unwrap().write(b) {
            Ok(n) => {
                self.pos += n as u64;
                let growth = self.pos.saturating_sub(self.len);
                self.len = self.len.max(self.pos);
                u.live += growth;
                u.peak = u.peak.max(u.live);
                u.until_check = u.until_check.saturating_sub(growth);
                Ok(n)
            }
            Err(e) => {
                u.error = Some(format!(
                    "scratch write at {}: {e}",
                    self.scratch.path().display()
                ));
                Err(e)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .unwrap()
            .flush()
            .map_err(|e| self.scratch.fail(e))
    }
}
impl Read for ScratchFile {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let n = self.file.as_mut().unwrap().read(b)?;
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for ScratchFile {
    fn seek(&mut self, s: SeekFrom) -> io::Result<u64> {
        self.pos = self.file.as_mut().unwrap().seek(s)?;
        Ok(self.pos)
    }
}
impl Drop for ScratchFile {
    fn drop(&mut self) {
        self.scratch.0.usage.lock().unwrap().live -= self.len;
    }
}

#[cfg(windows)]
pub struct MemoryGuard(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl MemoryGuard {
    pub fn install() -> Result<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::{JobObjects::*, Threading::GetCurrentProcess};
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error()).context("create Windows memory job");
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = 1_000_000_000;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const _ as *const std::ffi::c_void).cast(),
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(job, GetCurrentProcess()) == 0
            {
                let e = io::Error::last_os_error();
                CloseHandle(job);
                return Err(e).context("install 1 GB Windows process memory limit");
            }
            Ok(Self(job))
        }
    }
    pub fn stats(&self) -> String {
        use windows_sys::Win32::System::{ProcessStatus::*, Threading::GetCurrentProcess};
        unsafe {
            let mut m: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            m.cb = std::mem::size_of_val(&m) as u32;
            if GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) != 0 {
                format!(
                    "peak working set {} bytes, peak commit {} bytes",
                    m.PeakWorkingSetSize, m.PeakPagefileUsage
                )
            } else {
                "process metrics unavailable".into()
            }
        }
    }
}
#[cfg(windows)]
impl Drop for MemoryGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(not(windows))]
pub struct MemoryGuard;
#[cfg(not(windows))]
impl MemoryGuard {
    pub fn install() -> Result<Self> {
        Ok(Self)
    }
    pub fn stats(&self) -> String {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|x| {
                x.lines()
                    .find(|l| l.starts_with("VmHWM:"))
                    .map(str::to_owned)
            })
            .unwrap_or_default()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_tracks_growth_and_releases_on_drop() {
        let s = Scratch::new(None, 6).unwrap();
        let mut a = s.create().unwrap();
        a.write_all(b"abcd").unwrap();
        a.seek(SeekFrom::Start(0)).unwrap();
        a.write_all(b"00").unwrap();
        let mut b = s.create().unwrap();
        assert!(b.write_all(b"123").is_err());
        drop(a);
        b.write_all(b"12345").unwrap();
        assert_eq!(s.peak(), 5);
    }
}

/// Keep the file and its quota charge, releasing the OS handle between passes.
pub struct Parked {
    path: tempfile::TempPath,
    len: u64,
    scratch: Scratch,
}
impl Parked {
    pub fn open(&self) -> io::Result<File> {
        File::open(&self.path)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for Parked {
    fn drop(&mut self) {
        self.scratch.0.usage.lock().unwrap().live -= self.len;
    }
}
