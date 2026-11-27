//! Deterministic, handle-bounded local file discovery.
use super::{basename, supported};
use anyhow::{ensure, Context, Result};
use std::collections::{btree_map::Entry, BTreeMap};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Input {
    pub path: PathBuf,
    pub label: String,
}

// Covers the identity index, owned paths, sorting and the final list.
const MANIFEST_BUDGET: usize = 48 << 20;
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum FileId {
    Native(u64, u128),
    #[cfg(windows)]
    CanonicalPath(PathBuf),
}

#[cfg(unix)]
fn file_id(path: &Path) -> Result<FileId> {
    use std::os::unix::fs::MetadataExt;
    let info = std::fs::metadata(path)?;
    Ok(FileId::Native(info.dev(), u128::from(info.ino())))
}

#[cfg(windows)]
fn file_id(path: &Path) -> Result<FileId> {
    use std::mem::{size_of, MaybeUninit};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO,
    };
    let file = std::fs::File::open(path)?;
    let handle = file.as_raw_handle();
    let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
    // Both queries fully initialize their output on success. Drop the handle after
    // copying its ID so discovery can process more inputs than the open-file limit.
    unsafe {
        if GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        ) != 0
        {
            let info = info.assume_init();
            let id = u128::from_le_bytes(info.FileId.Identifier);
            if id != 0 {
                return Ok(FileId::Native(info.VolumeSerialNumber, id));
            }
        }
        let mut legacy = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        if GetFileInformationByHandle(handle, legacy.as_mut_ptr()) == 0 {
            return Ok(FileId::CanonicalPath(path.canonicalize()?));
        }
        let info = legacy.assume_init();
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        if index == 0 {
            return Ok(FileId::CanonicalPath(path.canonicalize()?));
        }
        Ok(FileId::Native(
            u64::from(info.dwVolumeSerialNumber),
            u128::from(index),
        ))
    }
}

fn cost(input: &Input) -> usize {
    384usize.saturating_add(
        input
            .path
            .capacity()
            .saturating_add(input.label.capacity())
            .saturating_mul(3),
    )
}

fn insert(
    files: &mut BTreeMap<FileId, Input>,
    used: &mut usize,
    path: &Path,
    label: String,
) -> Result<()> {
    if basename(path).starts_with("~$") {
        return Ok(());
    }
    let id = file_id(path).with_context(|| format!("identify input {}", path.display()))?;
    let candidate = Input {
        path: path.into(),
        label,
    };
    let entry = files.entry(id);
    let previous = match &entry {
        Entry::Occupied(old) => {
            if (&old.get().path, &old.get().label) <= (&candidate.path, &candidate.label) {
                return Ok(());
            }
            cost(old.get())
        }
        Entry::Vacant(_) => 0,
    };
    let next = used
        .saturating_sub(previous)
        .saturating_add(cost(&candidate));
    ensure!(
        next <= MANIFEST_BUDGET,
        "input path manifest exceeds its 48 MiB budget; select fewer files per run"
    );
    *used = next;
    match entry {
        Entry::Occupied(mut old) => {
            old.insert(candidate);
        }
        Entry::Vacant(slot) => {
            slot.insert(candidate);
        }
    }
    Ok(())
}

fn sorted_labels(mut files: Vec<Input>) -> Result<Vec<Input>> {
    files.sort_unstable_by(|a, b| a.label.cmp(&b.label).then(a.path.cmp(&b.path)));
    if !files.windows(2).any(|w| w[0].label == w[1].label) {
        return Ok(files);
    }

    // Identical explicit basenames across directories require a common relative root.
    let mut common = files.first().and_then(|f| f.path.parent());
    for file in &files {
        while let Some(p) = common {
            if file.path.starts_with(p) {
                break;
            }
            common = p.parent();
        }
    }
    let root = common.map(Path::to_path_buf);
    for (i, f) in files.iter_mut().enumerate() {
        f.label = if let Some(parent) = &root {
            f.path
                .strip_prefix(parent)?
                .to_string_lossy()
                .replace('\\', "/")
        } else {
            // Separate Windows volumes have no common ancestor.
            format!("[input-{:05}]/{}", i + 1, f.label)
        };
    }
    files.sort_unstable_by(|a, b| a.label.cmp(&b.label).then(a.path.cmp(&b.path)));
    ensure!(
        !files.windows(2).any(|w| w[0].label == w[1].label),
        "input file names collide when represented as Unicode; use distinct file names"
    );
    Ok(files)
}

pub fn discover(args: &[PathBuf], recursive: bool, scratch: &Path) -> Result<Vec<Input>> {
    let scratch = scratch
        .canonicalize()
        .unwrap_or_else(|_| scratch.to_owned());
    let mut files = BTreeMap::new();
    let mut used = 0;
    for arg in args {
        let real = arg
            .canonicalize()
            .with_context(|| format!("open input {}", arg.display()))?;
        if real.is_file() {
            ensure!(
                supported(&real),
                "unsupported input {} (use XLSX, CSV or TSV)",
                arg.display()
            );
            insert(&mut files, &mut used, &real, basename(&real))?;
            continue;
        }
        ensure!(
            real.is_dir(),
            "input is not a regular file or directory: {}",
            arg.display()
        );
        let walk = WalkDir::new(&real)
            .max_depth(if recursive { usize::MAX } else { 1 })
            .into_iter()
            .filter_entry(|e| {
                !e.file_type().is_dir()
                    || (e.path() != scratch && !e.path().join(".dataduck-export").exists())
            });
        for entry in walk {
            let entry = entry?;
            if entry.file_type().is_file() && supported(entry.path()) {
                let label = entry
                    .path()
                    .strip_prefix(&real)?
                    .to_string_lossy()
                    .replace('\\', "/");
                insert(&mut files, &mut used, entry.path(), label)?;
            }
        }
    }
    ensure!(!files.is_empty(), "no XLSX, CSV or TSV input files found");
    sorted_labels(files.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn repeated_paths_hard_links_and_directories_only_count_once() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        for name in ["a", "b", "scratch", "clean"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        fs::write(root.join("a/data.csv"), "a,n\nA,1").unwrap();
        fs::write(root.join("b/data.csv"), "a,n\nB,2").unwrap();
        fs::hard_link(root.join("a/data.csv"), root.join("alias.csv")).unwrap();
        fs::write(root.join("~$ignored.xlsx"), "").unwrap();
        fs::write(root.join("clean/.dataduck-export"), "").unwrap();
        fs::write(root.join("clean/export.csv"), "").unwrap();
        fs::write(root.join("scratch/temp.csv"), "").unwrap();
        let mut args = vec![
            root.to_owned(),
            root.join("alias.csv"),
            root.join("a/data.csv"),
            root.join("b/data.csv"),
        ];
        let load = |args: &[PathBuf]| {
            discover(args, true, &root.join("scratch"))
                .unwrap()
                .into_iter()
                .map(|f| f.label)
                .collect::<Vec<_>>()
        };
        let expected = load(&args);
        assert_eq!(expected.len(), 2);
        assert!(expected[0] != expected[1]);
        assert!(!expected.iter().any(|label| Path::new(label).is_absolute()));
        args.reverse();
        assert_eq!(load(&args), expected);
        let same_name = load(&[root.join("b/data.csv"), root.join("a/data.csv")]);
        assert_eq!(same_name, ["a/data.csv", "b/data.csv"]);
    }

    #[test]
    fn discovers_thousands_of_files_without_keeping_open_handles() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..2000 {
            fs::File::create(t.path().join(format!("{i:04}.csv"))).unwrap();
        }
        let inputs = discover(&[t.path().into()], false, &t.path().join("scratch")).unwrap();
        assert_eq!(inputs.len(), 2000);
        assert_eq!(inputs[0].label, "0000.csv");
        assert_eq!(inputs[1999].label, "1999.csv");
    }
}
