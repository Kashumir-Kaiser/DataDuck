//! Stage a new Power BI project or one immutable data snapshot on its destination volume.
use super::{facts, render, Contract};
use crate::resource::{check_cancel, FREE_RESERVE};
use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use tempfile::{NamedTempFile, TempDir};

const CONTRACT_LIMIT: u64 = 64 << 20;
const MODEL_LIMIT: usize = 16_000;
const IO_BYTES: usize = 256 << 10;
const MARKER: &str = ".dataduck-export";
const INITIAL: &str = "initial-snapshot.txt";
const TRACE: &str = "chart-decisions.ndjson";

pub struct Stage {
    destination: PathBuf,
    work: TempDir,
    snapshot: PathBuf,
    update: bool,
    html: Option<PathBuf>,
    baseline: Option<Baseline>,
    _lock: File,
}

pub struct Outcome {
    pub snapshot: PathBuf,
    pub bytes: u64,
}

fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

// First line remains the snapshot ID. The next two lines protect the contract
// and decision trace copied across otherwise independent snapshot generations.
struct Baseline {
    snapshot: String,
    contract: String,
    trace: String,
}

impl Baseline {
    fn read(internal: &Path) -> Result<Self> {
        let marker = internal.join(INITIAL);
        regular(&marker, false)?;
        let mut value = String::new();
        File::open(marker)?.take(196).read_to_string(&mut value)?;
        let ids: Vec<_> = value.split_terminator('\n').collect();
        ensure!(
            ids.len() == 3 && ids.iter().all(|id| valid_digest(id)),
            "invalid initial snapshot metadata; use powerbi export into a fresh folder"
        );
        Ok(Self {
            snapshot: ids[0].into(),
            contract: ids[1].into(),
            trace: ids[2].into(),
        })
    }
}

fn hash_copy(mut source: impl Read, mut dest: impl Write) -> Result<String> {
    let mut buffer = vec![0; IO_BYTES];
    let mut digest = Sha256::new();
    loop {
        check_cancel()?;
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
        dest.write_all(&buffer[..n])?;
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn valid_project_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|tail| {
            matches!(
                tail,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    !name.is_empty()
        && !reserved
        && !name.ends_with([' ', '.'])
        && name.encode_utf16().count() + ".SemanticModel".len() <= 255
        && !name
            .chars()
            .any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
}

fn unoccupied(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(_) => bail!("destination already exists: {}", path.display()),
    }
}

fn regular(path: &Path, directory: bool) -> Result<()> {
    let m = fs::symlink_metadata(path).with_context(|| format!("open {}", path.display()))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        ensure!(
            m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "unexpected reparse point: {}",
            path.display()
        );
    }
    ensure!(
        (directory && m.is_dir()) || (!directory && m.is_file()),
        "unexpected file type: {}",
        path.display()
    );
    Ok(())
}

fn free_space(path: &Path) -> Result<()> {
    check_cancel()?;
    ensure!(
        fs2::available_space(path)? >= FREE_RESERVE,
        "output volume needs 2 GiB free: {}",
        path.display()
    );
    Ok(())
}

fn project_lock(project: &Path) -> Result<File> {
    // Never unlink the lock inode. A waiting process could still hold the old inode
    // while another writer locks a replacement. The small lock lives outside exports.
    let lock_dir = std::env::temp_dir().join("dataduck-project-locks");
    fs::create_dir_all(&lock_dir)?;
    regular(&lock_dir, true)?;
    #[cfg(windows)]
    let path_key = project.to_string_lossy().to_lowercase().into_bytes();
    #[cfg(not(windows))]
    let path_key = project.as_os_str().as_encoded_bytes();
    let filename = format!("{:x}.lock", Sha256::digest(path_key));
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_dir.join(filename))?;
    fs2::FileExt::try_lock_exclusive(&f).with_context(|| {
        format!(
            "another DataDuck writer holds the project lock for {}",
            project.display()
        )
    })?;
    Ok(f)
}

// Renaming over an empty destination directory is permitted by POSIX rename().
// Use the platform's explicit no-replace operation for publication.
fn rename_new(from: &Path, to: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use rustix::fs::{renameat_with, RenameFlags, CWD};
        renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(io::Error::from)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let a: Vec<_> = from.as_os_str().encode_wide().chain([0]).collect();
        let b: Vec<_> = to.as_os_str().encode_wide().chain([0]).collect();
        // Both buffers are NUL-terminated paths and remain valid through the call.
        if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    bail!(
        "atomic no-overwrite directory publication is unsupported on this platform: {}",
        to.display()
    );
    Ok(())
}

fn portable_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(network) = value.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{network}"));
        }
        if let Some(drive) = value.strip_prefix(r"\\?\") {
            return PathBuf::from(drive);
        }
    }
    path.to_path_buf()
}

impl Stage {
    pub fn open(destination: &Path, update: bool, html: Option<&Path>) -> Result<Self> {
        let destination = crate::destination(destination)?;
        let name = destination
            .file_name()
            .and_then(OsStr::to_str)
            .context("project name must be valid Unicode")?;
        ensure!(
            valid_project_name(name),
            "invalid Windows project folder name: {name:?}"
        );
        let parent = destination
            .parent()
            .context("project needs a parent directory")?;
        let lock = project_lock(&destination)?;
        free_space(parent)?;
        ensure!(
            !update || html.is_none(),
            "--html is available only on a new export"
        );
        let html = html.map(crate::destination).transpose()?;
        if let Some(path) = &html {
            ensure!(
                !crate::same_dest(path, &destination) && !path.starts_with(&destination),
                "conflicting Power BI and HTML destinations"
            );
            unoccupied(path)?;
            free_space(path.parent().context("HTML needs a parent directory")?)?;
        }
        let internal = destination.join("DataDuck");
        let staging_parent = if update {
            regular(&destination, true)?;
            regular(&destination.join(MARKER), false)
                .context("project was not generated by DataDuck")?;
            regular(&internal, true)?;
            regular(&internal.join("snapshots"), true)?;
            &internal
        } else {
            unoccupied(&destination).context("use powerbi update-data for an existing project")?;
            parent
        };
        let baseline = if update {
            Some(Baseline::read(&internal)?)
        } else {
            None
        };
        let work = tempfile::Builder::new()
            .prefix(".dataduck-powerbi-")
            .tempdir_in(staging_parent)?;
        File::create(work.path().join(MARKER))?;
        let snapshot = if update {
            work.path().join("data")
        } else {
            work.path().join("DataDuck/snapshots/pending")
        };
        fs::create_dir_all(&snapshot)?;
        Ok(Self {
            destination,
            work,
            snapshot,
            update,
            html,
            baseline,
            _lock: lock,
        })
    }

    pub fn snapshot(&self) -> &Path {
        &self.snapshot
    }

    pub fn load_contract(&self) -> Result<Contract> {
        ensure!(self.update, "only data updates read an existing contract");
        let path = self.destination.join("DataDuck/contract.json");
        regular(&path, false)?;
        let mut file = File::open(&path)?;
        ensure!(
            file.metadata()?.len() <= CONTRACT_LIMIT,
            "DataDuck contract exceeds 64 MiB"
        );
        use std::io::Seek;
        let expected = &self
            .baseline
            .as_ref()
            .context("missing update baseline")?
            .contract;
        ensure!(
            &hash_copy((&mut file).take(CONTRACT_LIMIT + 1), io::sink())? == expected,
            "DataDuck project contract was modified; use powerbi export into a fresh folder"
        );
        file.rewind()?;
        let contract: Contract =
            serde_json::from_reader(BufReader::new(file.take(CONTRACT_LIMIT + 1)))
                .context("invalid DataDuck project contract")?;
        validate_contract(&contract)?;
        Ok(contract)
    }

    pub fn copy_initial_decisions(&self, dest: &mut impl Write) -> Result<()> {
        ensure!(self.update, "only data updates reuse saved chart decisions");
        let internal = self.destination.join("DataDuck");
        let baseline = self.baseline.as_ref().context("missing update baseline")?;
        let directory = internal.join("snapshots").join(&baseline.snapshot);
        regular(&directory, true)?;
        let trace = directory.join(TRACE);
        regular(&trace, false)?;
        ensure!(hash_copy(File::open(trace)?, dest)? == baseline.trace,
            "initial snapshot chart decisions were modified; use powerbi export into a fresh folder");
        Ok(())
    }

    pub fn finish(self, contract: &Contract, html: Option<NamedTempFile>) -> Result<Outcome> {
        ensure!(
            html.is_some() == self.html.is_some(),
            "HTML stage does not match requested output"
        );
        validate_contract(contract)?;
        validate_partitions(&self.snapshot, contract)?;
        let (id, bytes) = snapshot_digest(&self.snapshot)?;
        let final_snapshot = self.destination.join("DataDuck/snapshots").join(&id);
        let outcome = Outcome {
            bytes,
            snapshot: portable_path(&final_snapshot),
        };
        if self.update {
            let snapshots = final_snapshot.parent().unwrap();
            free_space(snapshots)?;
            match fs::symlink_metadata(&final_snapshot) {
                Ok(_) => {
                    regular(&final_snapshot, true)?;
                    ensure!(snapshot_digest(&final_snapshot)? == (id, bytes),
                        "existing snapshot was modified; keeping both the project and its existing data intact: {}", final_snapshot.display());
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    rename_new(&self.snapshot, &final_snapshot).with_context(|| {
                        format!("publish snapshot {}", final_snapshot.display())
                    })?;
                }
                Err(e) => return Err(e.into()),
            }
            return Ok(outcome);
        }

        let internal = self.work.path().join("DataDuck");
        let snapshot = internal.join("snapshots").join(&id);
        rename_new(&self.snapshot, &snapshot)?;
        let contract_path = internal.join("contract.json");
        write_contract(&contract_path, contract)?;
        let contract_hash = hash_copy(File::open(contract_path)?, io::sink())?;
        let trace_hash = hash_copy(File::open(snapshot.join(TRACE))?, io::sink())?;
        let mut initial = File::create(internal.join(INITIAL))?;
        writeln!(initial, "{id}\n{contract_hash}\n{trace_hash}")?;
        initial.sync_all()?;
        drop(initial);
        let name = self
            .destination
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap();
        render::render(self.work.path(), name, &outcome.snapshot, contract)?;
        regular(&self.work.path().join(format!("{name}.pbip")), false)?;
        free_space(self.work.path())?;
        check_cancel()?;
        // Commit HTML first. If directory publication loses a race, roll back only
        // this newly created file. Never recurse into a conflicting project folder.
        let html_file = match (html, self.html.as_deref()) {
            (Some(stage), Some(path)) => {
                let identity = same_file::Handle::from_file(stage.as_file().try_clone()?)?;
                stage.as_file().sync_all()?;
                stage
                    .persist_noclobber(path)
                    .with_context(|| format!("publish {}", path.display()))?;
                Some((path, identity))
            }
            _ => None,
        };
        if let Err(e) = rename_new(self.work.path(), &self.destination) {
            if let Some((path, identity)) = html_file {
                let own =
                    same_file::Handle::from_path(path).is_ok_and(|current| current == identity);
                drop(identity);
                if own {
                    fs::remove_file(path).with_context(|| {
                        format!("could not roll back HTML {} after {e:#}", path.display())
                    })?;
                }
            }
            return Err(e)
                .with_context(|| format!("publish project to {}", self.destination.display()));
        }
        // The temporary path has been moved; disarm cleanup so that it can never
        // remove a different directory created under its old staging name.
        let _ = self.work.keep();
        Ok(outcome)
    }
}

fn validate_contract(contract: &Contract) -> Result<()> {
    ensure!(
        contract.version == 1 && contract.template_version == 1,
        "unsupported DataDuck Power BI contract version"
    );
    ensure!(
        !contract.tables.is_empty(),
        "empty DataDuck Power BI contract"
    );
    let mut ids = HashSet::new();
    let mut model_size = 3 + 11 + 4 + 7; // Three audit tables and their fields.
    for table in &contract.tables {
        ensure!(
            table.id.strip_prefix("t_").is_some_and(valid_digest) && ids.insert(&table.id),
            "invalid or repeated contract table ID: {:?}",
            table.id
        );
        ensure!(
            !table.fields.is_empty(),
            "empty contract table: {}",
            table.name
        );
        // Count every fact table, fact field, source coordinates, axis key and axis table.
        model_size += 1 + table.fields.len() + 2 + table.axes.len() * 6;
        ensure!(
            model_size <= MODEL_LIMIT,
            "project exceeds the Power BI model-wide limit of 16,000 columns and tables"
        );
        let mut keys = HashSet::new();
        for f in &table.fields {
            ensure!(
                !f.key.is_empty()
                    && f.key == crate::layout::header_key(&f.name)
                    && keys.insert(&f.key),
                "invalid or repeated contract field key in {}",
                table.name
            );
        }
        ensure!(
            table.charts.len() <= 8,
            "too many saved charts on {}",
            table.name
        );
        let mut axes = HashSet::new();
        for a in &table.axes {
            ensure!(
                a.field < table.fields.len() && axes.insert(a.field),
                "invalid or duplicate axis in {}",
                table.name
            );
        }
        for c in &table.charts {
            ensure!(
                axes.contains(&c.axis)
                    && c.measure.is_none_or(|m| m < table.fields.len())
                    && c.measure.is_some() == c.aggregate.is_some(),
                "invalid saved chart binding on {}",
                table.name
            );
        }
    }
    Ok(())
}

fn validate_partitions(dir: &Path, contract: &Contract) -> Result<()> {
    for fixed in ["profile.csv", "missing.csv", "sources.csv", TRACE] {
        regular(&dir.join(fixed), false)?;
    }
    for table in &contract.tables {
        check_cancel()?;
        regular(&dir.join(facts::fact_file(&table.id)), false)?;
        for axis in &table.axes {
            regular(&dir.join(facts::axis_file(&table.id, axis.field)), false)?;
        }
    }
    Ok(())
}

fn write_contract(path: &Path, contract: &Contract) -> Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    serde_json::to_writer(&mut out, contract)?;
    out.write_all(b"\n")?;
    out.flush()?;
    ensure!(
        out.get_ref().metadata()?.len() <= CONTRACT_LIMIT,
        "DataDuck contract exceeds 64 MiB"
    );
    out.get_ref().sync_all()?;
    Ok(())
}

fn snapshot_digest(dir: &Path) -> Result<(String, u64)> {
    regular(dir, true)?;
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        check_cancel()?;
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid snapshot filename"))?;
        ensure!(files.len() < MODEL_LIMIT + 16, "too many files in snapshot");
        files.push(name);
    }
    files.sort_unstable();
    ensure!(
        files.iter().any(|file| file == TRACE),
        "incomplete snapshot: chart decisions missing"
    );
    let mut digest = Sha256::new();
    digest.update(b"DataDuck snapshot\0v1\0");
    let mut bytes: u64 = 0;
    let mut buf = vec![0; IO_BYTES];
    for name in files {
        let path = dir.join(&name);
        regular(&path, false)?;
        let mut file = File::open(&path)?;
        let size = file.metadata()?.len();
        bytes = bytes.checked_add(size).context("snapshot size overflow")?;
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update(size.to_le_bytes());
        let mut read = 0u64;
        loop {
            check_cancel()?;
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            read += n as u64;
            digest.update(&buf[..n]);
        }
        ensure!(
            read == size,
            "snapshot file size changed: {}",
            path.display()
        );
    }
    Ok((format!("{:x}", digest.finalize()), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_identity_covers_filenames_and_data() -> Result<()> {
        let a = tempfile::tempdir()?;
        let b = tempfile::tempdir()?;
        for (dir, order) in [
            (a.path(), ["facts.csv", TRACE]),
            (b.path(), [TRACE, "facts.csv"]),
        ] {
            for name in order {
                fs::write(dir.join(name), b"123")?;
            }
        }
        let id = snapshot_digest(a.path())?;
        assert_eq!(id, snapshot_digest(b.path())?);
        assert_eq!(id.1, 6);
        fs::write(b.path().join("facts.csv"), b"124")?;
        assert_ne!(id, snapshot_digest(b.path())?);
        fs::write(b.path().join("facts.csv"), b"123")?;
        fs::rename(b.path().join("facts.csv"), b.path().join("renamed.csv"))?;
        assert_ne!(id, snapshot_digest(b.path())?);
        Ok(())
    }

    #[test]
    fn writer_lock_and_noclobber_protect_even_an_empty_destination() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let dest = parent.path().join("Report");
        let guard = project_lock(&dest)?;
        assert!(project_lock(&dest).is_err());
        drop(guard);
        let _ = project_lock(&dest)?;
        let source = parent.path().join("stage");
        fs::create_dir(&source)?;
        fs::create_dir(&dest)?;
        assert!(rename_new(&source, &dest).is_err());
        assert!(source.exists() && dest.exists());
        Ok(())
    }

    fn fixture(stage: &Stage) -> Result<Contract> {
        let id = format!("t_{}", "a".repeat(64));
        for file in [
            "profile.csv",
            "missing.csv",
            "sources.csv",
            TRACE,
            &facts::fact_file(&id),
        ] {
            fs::write(stage.snapshot().join(file), b"fixture\n")?;
        }
        Ok(Contract {
            version: 1,
            template_version: 1,
            tables: vec![super::super::Table {
                id,
                name: "Table 1".into(),
                fields: vec![super::super::Field {
                    key: "city".into(),
                    name: "City".into(),
                    kind: crate::profile::Type::Text,
                    import: crate::profile::ImportType::Text,
                    utc: false,
                }],
                axes: vec![],
                charts: vec![],
            }],
        })
    }

    #[test]
    fn update_refuses_modified_contract_or_initial_decisions() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let dest = parent.path().join("Project");
        let stage = Stage::open(&dest, false, None)?;
        let contract = fixture(&stage)?;
        let published = stage.finish(&contract, None)?;
        let stage = Stage::open(&dest, true, None)?;
        stage.load_contract()?;
        let mut baseline = Vec::new();
        stage.copy_initial_decisions(&mut baseline)?;
        assert_eq!(baseline, b"fixture\n");

        fs::write(published.snapshot.join(TRACE), b"changed\n")?;
        let error = stage.copy_initial_decisions(&mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("were modified"), "{error}");
        let path = dest.join("DataDuck/contract.json");
        let mut modified = fs::read(&path)?;
        modified.push(b' ');
        fs::write(path, modified)?;
        assert!(stage
            .load_contract()
            .unwrap_err()
            .to_string()
            .contains("contract was modified"));
        Ok(())
    }

    #[test]
    fn incomplete_partitions_or_concurrent_directory_leave_no_html() -> Result<()> {
        let root = tempfile::tempdir()?;
        let dest = root.path().join("Project");
        let html = root.path().join("report.html");
        let stage = Stage::open(&dest, false, Some(&html))?;
        let contract = fixture(&stage)?;
        fs::remove_file(stage.snapshot().join("profile.csv"))?;
        assert!(stage
            .finish(&contract, Some(NamedTempFile::new_in(root.path())?))
            .is_err());
        assert!(!dest.exists() && !html.exists());

        let stage = Stage::open(&dest, false, Some(&html))?;
        let contract = fixture(&stage)?;
        fs::create_dir(&dest)?;
        assert!(stage
            .finish(&contract, Some(NamedTempFile::new_in(root.path())?))
            .is_err());
        assert!(dest.is_dir() && dest.read_dir()?.next().is_none());
        assert!(!html.exists());
        Ok(())
    }

    #[test]
    fn portable_component_names() {
        for invalid in [
            "",
            "PRN",
            "nul.pbip",
            "x|y",
            "a\\b",
            "rest.",
            "lpt2.data",
            "CONIN$",
        ] {
            assert!(!valid_project_name(invalid), "{invalid}");
        }
        assert!(valid_project_name("Giá & sales 2026"));
    }
}
