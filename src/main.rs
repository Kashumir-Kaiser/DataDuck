mod delimited;
mod discovery;
mod layout;
mod manifest;
mod model;
mod powerbi;
mod profile;
#[cfg(windows)]
mod prompt;
mod report;
mod resource;
mod segment;
mod sort;
mod xlsx;

use anyhow::{ensure, Context, Result};
use clap::{Args, Parser, Subcommand};
use discovery::{discover, Input};
use layout::{Config, Layout, SourceRule};
use model::{Event, Issue, Kind, Row, Source};
use profile::{Inference, Profiler};
use resource::{Scratch, ScratchFile};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use tempfile::{NamedTempFile, TempDir};

#[derive(Parser)]
#[command(
    version,
    about = "Clean local spreadsheets and create one offline profiling dashboard"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Clean, profile and publish a standalone dashboard.
    Build(Build),
    /// Export or refresh an editable Power BI Desktop project.
    #[command(name = "powerbi", subcommand)]
    PowerBi(powerbi::Command),
    /// Preview original rows, sheet names, and header detection.
    Inspect(Inspect),
}
#[derive(Args)]
struct Common {
    /// Versioned TOML source rules.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Directory for intermediate data (default: system temp).
    #[arg(long)]
    temp_dir: Option<PathBuf>,
    /// Maximum live scratch in GiB (default: 24).
    #[arg(long,value_parser=clap::value_parser!(u64).range(1..=1_048_576))]
    temp_limit_gib: Option<u64>,
}
#[derive(Args)]
struct Build {
    #[arg(required = true)]
    inputs: Vec<PathBuf>,
    #[arg(short, long, default_value = "dashboard.html")]
    output: PathBuf,
    /// Descend into input subdirectories.
    #[arg(long)]
    recursive: bool,
    /// Replace an existing dashboard after successful processing.
    #[arg(long)]
    overwrite: bool,
    /// Show only errors and final report location.
    #[arg(long)]
    quiet: bool,
    /// Export cleaned CSVs into a new directory.
    #[arg(long)]
    clean_dir: Option<PathBuf>,
    /// Export diagnostics to a new NDJSON file.
    #[arg(long)]
    issues: Option<PathBuf>,
    /// Maximum standalone HTML size in MiB (default: 32).
    #[arg(long,value_parser=clap::value_parser!(u64).range(1..=1_048_576))]
    max_html_mib: Option<u64>,
    #[command(flatten)]
    common: Common,
}
#[derive(Args)]
struct Inspect {
    input: PathBuf,
    #[arg(long)]
    sheet: Option<String>,
    #[arg(long,default_value_t=20,value_parser=clap::value_parser!(u32).range(1..=100))]
    rows: u32,
    #[command(flatten)]
    common: Common,
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn extension(p: &Path) -> String {
    p.extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase()
}
fn supported(p: &Path) -> bool {
    matches!(extension(p).as_str(), "csv" | "tsv" | "xlsx")
}
fn basename(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
fn delimiter(p: &Path, r: &SourceRule) -> u8 {
    r.delimiter
        .as_ref()
        .map(|d| d.as_bytes()[0])
        .unwrap_or(if extension(p) == "tsv" { b'\t' } else { b',' })
}
fn read_text(p: &Path, r: &SourceRule, f: impl FnMut(Event) -> Result<bool>) -> Result<String> {
    delimited::read(p, delimiter(p, r), r.encoding.as_deref(), f)
}

fn row_bytes(r: &Row) -> usize {
    std::mem::size_of::<Row>()
        + r.cells.capacity() * std::mem::size_of::<model::Cell>()
        + r.cells.iter().map(|c| c.value.capacity()).sum::<usize>()
}
fn trim_bytes(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.shrink_to_fit();
    }
}
fn append_issue(w: &mut impl Write, s: &Source, e: &Issue) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Record<'a> {
        path: &'a str,
        sheet: Option<&'a str>,
        #[serde(flatten)]
        issue: &'a Issue,
    }
    serde_json::to_writer(
        &mut *w,
        &Record {
            path: &s.path,
            sheet: s.sheet.as_deref(),
            issue: e,
        },
    )?;
    w.write_all(b"\n")?;
    Ok(())
}
struct Audit<'a> {
    source: Source,
    file: Option<BufWriter<ScratchFile>>,
    scratch: &'a Scratch,
    log: bool,
}
impl Audit<'_> {
    fn add(&mut self, mut e: Issue) -> Result<()> {
        trim_bytes(&mut e.excerpt, 256);
        trim_bytes(&mut e.message, 1024);
        *self.source.issues.entry(e.code.clone()).or_default() += 1;
        if e.loss {
            self.source.status = "partial".into();
            if matches!(
                e.code.as_str(),
                "extra_fields"
                    | "oversized_row"
                    | "cell_too_large"
                    | "too_many_columns"
                    | "row_too_large"
                    | "invalid_encoding"
                    | "trailing_quoted_data"
            ) {
                self.source.rejected_rows += 1;
            }
        }
        if self.log {
            if self.file.is_none() {
                self.file = Some(BufWriter::new(self.scratch.create()?));
            }
            append_issue(self.file.as_mut().unwrap(), &self.source, &e)?;
        }
        if (e.loss || !matches!(e.code.as_str(), "header" | "preamble" | "outside_range"))
            && self.source.examples.len() < 12
        {
            self.source.examples.push(e);
        }
        Ok(())
    }
}
struct Capture<'a> {
    rule: &'a SourceRule,
    audit: Audit<'a>,
    probe: Vec<Row>,
    probe_bytes: usize,
    layout: Option<Layout>,
    inference: Option<Inference>,
    writer: BufWriter<ScratchFile>,
}
impl Capture<'_> {
    fn initialize(&mut self) -> Result<()> {
        let layout = layout::detect(&self.probe, self.rule)?;
        self.audit.source.layout = layout.description.clone();
        self.inference = Some(Inference::new(layout.headers.len()));
        self.layout = Some(layout);
        for r in std::mem::take(&mut self.probe) {
            self.accept(r)?;
        }
        Ok(())
    }
    fn accept(&mut self, r: Row) -> Result<()> {
        if let Some(row) = self
            .layout
            .as_ref()
            .unwrap()
            .clean(r, self.rule, &mut |e| self.audit.add(e))?
        {
            self.inference.as_mut().unwrap().observe(&row)?;
            segment::append(&mut self.writer, &row)?;
            self.audit.source.rows += 1;
        }
        Ok(())
    }
    fn event(&mut self, event: Event) -> Result<bool> {
        resource::check_cancel()?;
        let r = match event {
            Event::Issue(e) => {
                self.audit.add(e)?;
                return Ok(true);
            }
            Event::Row(r) => r,
        };
        if self.layout.is_some() {
            self.accept(r)?;
            return Ok(true);
        }
        let headers = &self.rule.header_rows;
        if let Some(last) = headers.last() {
            if r.number <= *last && headers.binary_search(&r.number).is_err() {
                self.audit.add(Issue {
                    row: r.number,
                    line_start: r.line_start,
                    line_end: r.line_end,
                    code: "preamble".into(),
                    message: "before configured header".into(),
                    ..Default::default()
                })?;
                return Ok(true);
            }
            self.probe_bytes += row_bytes(&r);
            ensure!(
                self.probe_bytes <= 16 << 20,
                "configured header rows exceed 16 MiB"
            );
            let ready = r.number >= *last;
            self.probe.push(r);
            if ready {
                self.initialize()?;
            }
            return Ok(true);
        }
        if !self.rule.headerless
            && r.cells
                .iter()
                .all(|c| c.kind == Kind::Missing || c.value.trim().is_empty())
            && self.probe.is_empty()
        {
            self.audit.add(Issue {
                row: r.number,
                line_start: r.line_start,
                line_end: r.line_end,
                code: "blank_row".into(),
                message: "empty row before header".into(),
                ..Default::default()
            })?;
            return Ok(true);
        }
        self.probe_bytes += row_bytes(&r);
        self.probe.push(r);
        if self.probe.len() >= 32 || self.probe_bytes >= 2 << 20 {
            self.initialize()?;
        }
        Ok(true)
    }
}
struct Part {
    rows: resource::Parked,
    hints: resource::Parked,
    source: String,
    path_len: usize,
}
struct Group {
    name: String,
    headers: resource::Parked,
    parts: Vec<Part>,
    accepted: u64,
}
struct Dataset {
    quiet: bool,
    sources: manifest::Catalog,
    groups: Vec<Group>,
    keys: HashMap<u64, Vec<usize>>,
}
impl Dataset {
    fn skip(
        &mut self,
        path: &str,
        sheet: Option<&str>,
        e: anyhow::Error,
        issues: &mut Option<IssueWriter>,
    ) -> Result<()> {
        let mut message = format!("{e:#}");
        trim_bytes(&mut message, 2048);
        let s = Source {
            path: path.into(),
            sheet: sheet.map(str::to_string),
            status: "skipped".into(),
            message: message.clone(),
            issues: BTreeMap::from([("source_skipped".into(), 1)]),
            ..Default::default()
        };
        eprintln!("Skipped {path} {}: {message}", sheet.unwrap_or(""));
        if let Some(w) = issues {
            append_issue(
                w,
                &s,
                &Issue {
                    code: "source_skipped".into(),
                    message,
                    loss: true,
                    ..Default::default()
                },
            )?;
        }
        self.sources.push(&s)
    }
    fn capture(
        &mut self,
        path: &str,
        sheet: Option<&str>,
        rule: &SourceRule,
        scratch: &Scratch,
        issues: &mut Option<IssueWriter>,
        read: impl FnOnce(&mut dyn FnMut(Event) -> Result<bool>) -> Result<String>,
    ) -> Result<()> {
        let source = Source {
            path: path.into(),
            sheet: sheet.map(str::to_string),
            status: "complete".into(),
            rules: settings(rule)?,
            ..Default::default()
        };
        let mut c = Capture {
            rule,
            audit: Audit {
                source,
                file: None,
                scratch,
                log: issues.is_some(),
            },
            probe: Vec::new(),
            probe_bytes: 0,
            layout: None,
            inference: None,
            writer: BufWriter::with_capacity(256 << 10, scratch.create()?),
        };
        c.writer.write_all(segment::MAGIC)?;
        let mut last_progress = std::time::Instant::now();
        let mut events = 0u64;
        let result = read(&mut |ev| {
            events += 1;
            if !self.quiet
                && events.is_multiple_of(16_384)
                && last_progress.elapsed().as_secs() >= 2
            {
                eprintln!(
                    "Reading {path} {}: {} accepted rows, peak scratch {} bytes",
                    sheet.unwrap_or(""),
                    c.audit.source.rows,
                    scratch.peak()
                );
                last_progress = std::time::Instant::now();
            }
            c.event(ev)
        })
        .and_then(|encoding| {
            c.audit.source.encoding = encoding;
            if c.layout.is_none() {
                c.initialize()?;
            }
            Ok(())
        });
        scratch.check()?;
        if let Err(e) = result {
            return self.skip(path, sheet, e, issues);
        }
        c.writer.flush()?;
        if let (Some(mut local), Some(out)) = (c.audit.file, issues.as_mut()) {
            local.flush()?;
            io::copy(&mut local.get_ref().reopen()?, out)?;
        }
        let headers = c.layout.unwrap().headers;
        let mut signature: Vec<_> = headers.iter().map(|h| layout::header_key(h)).collect();
        signature.sort();
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        signature.hash(&mut hash);
        let key = hash.finish();
        let mut matched = None;
        for &i in self.keys.get(&key).into_iter().flatten() {
            let existing: Vec<String> =
                serde_json::from_reader(BufReader::new(self.groups[i].headers.open()?))?;
            let mut keys: Vec<_> = existing.iter().map(|h| layout::header_key(h)).collect();
            keys.sort();
            if keys == signature {
                matched = Some((i, existing));
                break;
            }
        }
        let (id, group_headers) = if let Some(pair) = matched {
            pair
        } else {
            let i = self.groups.len();
            let file = store_json(scratch, &headers)?;
            self.groups.push(Group {
                name: format!("Table {}", i + 1),
                headers: file,
                parts: vec![],
                accepted: 0,
            });
            self.keys.entry(key).or_default().push(i);
            (i, headers.clone())
        };
        let g = &mut self.groups[id];
        let order: HashMap<_, _> = group_headers
            .iter()
            .enumerate()
            .map(|(i, h)| (layout::header_key(h), i))
            .collect();
        let map: Vec<_> = headers
            .iter()
            .map(|h| order[&layout::header_key(h)])
            .collect();
        let hints = store_json(scratch, &(map, c.inference.unwrap()))?;
        c.audit.source.table = Some(g.name.clone());
        let source = match sheet {
            Some(n) => format!("{path} / {n}"),
            None => path.into(),
        };
        g.accepted += c.audit.source.rows;
        g.parts.push(Part {
            path_len: path.len(),
            rows: c.writer.into_inner().map_err(|e| e.into_error())?.park()?,
            hints,
            source,
        });
        self.sources.push(&c.audit.source)
    }
}
fn store_json<T: serde::Serialize>(s: &Scratch, value: &T) -> Result<resource::Parked> {
    let mut w = BufWriter::new(s.create()?);
    serde_json::to_writer(&mut w, value)?;
    w.flush()?;
    Ok(w.into_inner().map_err(|e| e.into_error())?.park()?)
}
fn settings(rule: &SourceRule) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(rule)?;
    if let Some(fields) = value.as_object_mut() {
        fields.retain(|_, v| {
            !(v.is_null()
                || v.as_array().is_some_and(Vec::is_empty)
                || v.as_object().is_some_and(|m| m.is_empty())
                || v.as_str() == Some("")
                || v.as_bool() == Some(false))
        });
    }
    Ok(value)
}
#[derive(Debug)]
struct InvalidSettings;
impl std::fmt::Display for InvalidSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid configuration")
    }
}
impl std::error::Error for InvalidSettings {}
fn config(common: &Common) -> Result<Config> {
    match &common.config {
        Some(p) => Config::load(p).context(InvalidSettings),
        None => Ok(Config::default()),
    }
}
fn scratch(common: &Common, c: &Config) -> Result<Scratch> {
    let limit = common.temp_limit_gib.or(c.temp_limit_gib).unwrap_or(24);
    ensure!(limit > 0, "--temp-limit-gib must be positive");
    Scratch::new(
        common.temp_dir.as_deref().or(c.temp_dir.as_deref()),
        limit
            .checked_mul(1 << 30)
            .context("scratch limit too large")?,
    )
}

fn effective_rule(
    workbook: &mut xlsx::Workbook,
    index: usize,
    configured: Option<&SourceRule>,
) -> Result<SourceRule> {
    let mut r = configured.cloned().unwrap_or_default();
    if r.header_rows.is_empty() && !r.headerless && r.column_range.is_none() && r.names.is_none() {
        if let Some(t) = workbook.table(index)? {
            r.header_rows = vec![t.header];
            r.names = Some(t.names);
            r.column_range = Some([t.first, t.last]);
            r.data_end_row = r.data_end_row.or(Some(t.end));
        }
    }
    if r.header_rows.len() > 1 {
        workbook.prepare_headers(index, &r.header_rows)?;
    }
    Ok(r)
}

fn inspect(o: Inspect) -> Result<()> {
    let cfg = config(&o.common)?;
    let s = scratch(&o.common, &cfg)?;
    let f = o.input.canonicalize()?;
    let label = basename(&f);
    let preview = |rule: &SourceRule,
                   read: &mut dyn FnMut(&mut dyn FnMut(Event) -> Result<bool>) -> Result<()>|
     -> Result<()> {
        let mut probe = Vec::new();
        let mut size = 0usize;
        let mut shown = 0;
        let needed = rule.header_rows.last().copied().unwrap_or(0);
        read(&mut |ev| {
            match ev {
                Event::Issue(e) => println!("  row {} [{}] {}", e.row, e.code, e.message),
                Event::Row(row) => {
                    if shown < o.rows {
                        let values: Vec<_> = row
                            .cells
                            .iter()
                            .take(20)
                            .map(|c| {
                                let value = c.value.chars().take(100).collect::<String>();
                                format!("{}={value:?}", c.col + 1)
                            })
                            .collect();
                        println!("  {:>7}  {}", row.number, values.join("  "));
                        shown += 1;
                    }
                    let number = row.number;
                    let take = if needed > 0 {
                        rule.header_rows.contains(&number)
                    } else {
                        probe.len() < 32
                            && size < 2 << 20
                            && row.cells.iter().any(|c| !c.value.trim().is_empty())
                    };
                    if take {
                        size += row_bytes(&row);
                        ensure!(size <= 32 << 20, "inspection headers too large");
                        probe.push(row);
                    }
                    if shown >= o.rows
                        && number >= needed
                        && (needed > 0 || probe.len() >= 32 || size >= 2 << 20)
                    {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        })?;
        let suggested = match layout::detect(&probe, rule) {
            Ok(l) => {
                println!("  {}\n  Columns: {}", l.description, l.headers.join(" | "));
                l.header_coordinates().to_vec()
            }
            Err(e) => {
                println!("  Header needs a recipe: {e:#}");
                vec![probe.first().map_or(1, |r| r.number)]
            }
        };
        println!(
            "  Example override:\n    [[source]]\n    path = {:?}",
            label
        );
        if let Some(sheet) = &rule.sheet {
            println!("    sheet = {sheet:?}");
        }
        if suggested.is_empty() {
            println!("    headerless = true")
        } else {
            println!("    header_rows = {suggested:?}")
        };
        Ok(())
    };
    if extension(&f) == "xlsx" {
        let mut w = xlsx::Workbook::open(&f, &s)?;
        if let Some(name) = &o.sheet {
            ensure!(
                w.sheets().iter().any(|x| &x.name == name),
                "unknown sheet {name}"
            );
        }
        for i in 0..w.sheets().len() {
            let name = w.sheets()[i].name.clone();
            let hidden = w.sheets()[i].hidden;
            println!("Sheet {name:?}{}", if hidden { " (hidden)" } else { "" });
            if o.sheet.as_ref().is_some_and(|sel| sel != &name) || hidden && o.sheet.is_none() {
                continue;
            }
            let mut rule = effective_rule(
                &mut w,
                i,
                cfg.rule_for(&label, Some(&name)).context(InvalidSettings)?,
            )?;
            rule.sheet = Some(name);
            preview(&rule, &mut |cb| w.read(i, cb))?;
        }
    } else {
        ensure!(
            supported(&f) && o.sheet.is_none(),
            "CSV/TSV inspection requires a local file and no --sheet"
        );
        let default = SourceRule::default();
        let rule = cfg
            .rule_for(&label, None)
            .context(InvalidSettings)?
            .unwrap_or(&default);
        println!("File {label}");
        preview(rule, &mut |cb| {
            read_text(&f, rule, cb)?;
            Ok(())
        })?;
    }
    Ok(())
}

fn destination(p: &Path) -> Result<PathBuf> {
    let name = p
        .file_name()
        .context("output needs a file or directory name")?;
    Ok(parent(p)
        .canonicalize()
        .with_context(|| format!("destination parent {}", parent(p).display()))?
        .join(name))
}
fn same_dest(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    #[cfg(windows)]
    if a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
    {
        return true;
    }
    a.exists() && b.exists() && same_file::is_same_file(a, b).unwrap_or(false)
}
struct OutputPaths {
    html: PathBuf,
    clean: Option<PathBuf>,
    issues: Option<PathBuf>,
}
impl OutputPaths {
    fn validate(o: &Build, files: &[Input]) -> Result<Self> {
        let html = destination(&o.output)?;
        let clean = o.clean_dir.as_deref().map(destination).transpose()?;
        let issues = o.issues.as_deref().map(destination).transpose()?;
        ensure!(
            !html.exists() || (o.overwrite && html.is_file()),
            "dashboard exists; use --overwrite: {}",
            html.display()
        );
        let mut paths = vec![&html];
        for p in [&clean, &issues].into_iter().flatten() {
            ensure!(
                !p.exists(),
                "export destination already exists: {}",
                p.display()
            );
            ensure!(
                !paths.iter().any(|q| same_dest(p, q)),
                "conflicting output destinations: {}",
                p.display()
            );
            paths.push(p);
        }
        for p in paths {
            ensure!(
                !files.iter().any(|f| same_dest(p, &f.path)),
                "output overlaps input: {}",
                p.display()
            );
            ensure!(
                fs2::available_space(parent(p))? >= resource::FREE_RESERVE,
                "output volume needs 2 GiB free: {}",
                p.display()
            );
        }
        Ok(Self {
            html,
            clean,
            issues,
        })
    }
}
/// Verify free space on the destination volume periodically as large exports grow.
type IssueWriter = BufWriter<VolumeWriter<NamedTempFile>>;
struct VolumeWriter<W> {
    inner: W,
    dir: PathBuf,
    until: u64,
}
impl<W: Write> Write for VolumeWriter<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        resource::check_cancel()?;
        if self.until <= b.len() as u64 {
            if fs2::available_space(&self.dir)? < resource::FREE_RESERVE + b.len() as u64 {
                return Err(io::Error::other(format!(
                    "output volume needs 2 GiB free: {}",
                    self.dir.display()
                )));
            }
            self.until = 4 << 20;
        }
        let n = self.inner.write(b)?;
        self.until = self.until.saturating_sub(n as u64);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
fn remap(r: &mut Row, m: &[usize]) {
    for c in &mut r.cells {
        c.col = m[c.col]
    }
    r.cells.sort_unstable_by_key(|c| c.col);
}
fn write_csv(w: &mut csv::Writer<VolumeWriter<File>>, r: &Row) -> Result<()> {
    let mut cells = r.cells.iter().peekable();
    w.write_record((0..r.width).map(|i| match cells.peek() {
        Some(c) if c.col == i => {
            let c = cells.next().unwrap();
            if c.kind == Kind::Missing {
                ""
            } else {
                c.value.as_str()
            }
        }
        _ => "",
    }))?;
    Ok(())
}
fn ingest(
    files: Vec<Input>,
    cfg: &Config,
    s: &Scratch,
    quiet: bool,
    issues: &mut Option<IssueWriter>,
) -> Result<Dataset> {
    let mut data = Dataset {
        quiet,
        sources: manifest::Catalog::new(s)?,
        groups: Vec::new(),
        keys: HashMap::new(),
    };
    for f in files {
        s.check()?;
        if !quiet {
            eprintln!("Reading {} ({} bytes)", f.label, f.path.metadata()?.len());
        }
        if extension(&f.path) == "xlsx" {
            let mut w = match xlsx::Workbook::open(&f.path, s) {
                Ok(w) => w,
                Err(e) => {
                    s.check()?;
                    data.skip(&f.label, None, e, issues)?;
                    continue;
                }
            };
            for i in 0..w.sheets().len() {
                let name = w.sheets()[i].name.clone();
                let rule = cfg
                    .rule_for(&f.label, Some(&name))
                    .context(InvalidSettings)?;
                if w.sheets()[i].hidden && rule.is_none_or(|r| r.sheet.is_none()) {
                    continue;
                }
                let rule = match effective_rule(&mut w, i, rule) {
                    Ok(r) => r,
                    Err(e) => {
                        s.check()?;
                        data.skip(&f.label, Some(&name), e, issues)?;
                        continue;
                    }
                };
                data.capture(&f.label, Some(&name), &rule, s, issues, |cb| {
                    w.read(i, cb)?;
                    Ok("XLSX".into())
                })?;
            }
        } else {
            let default = SourceRule::default();
            let r = cfg
                .rule_for(&f.label, None)
                .context(InvalidSettings)?
                .unwrap_or(&default);
            data.capture(&f.label, None, r, s, issues, |cb| read_text(&f.path, r, cb))?;
        }
    }
    ensure!(
        !data.groups.is_empty(),
        "no usable source; inspect the inputs and source diagnostics above"
    );
    data.sources.seal()?;
    data.keys.clear();
    data.keys.shrink_to_fit();
    Ok(data)
}

impl Part {
    fn replay(
        &self,
        order: Option<&[usize]>,
        mut visit: impl FnMut(&Row) -> Result<()>,
    ) -> Result<()> {
        let (mut map, _): (Vec<usize>, serde::de::IgnoredAny) =
            serde_json::from_reader(BufReader::new(self.hints.open()?))?;
        if let Some(order) = order {
            for col in &mut map {
                *col = order[*col];
            }
        }
        let same = map.iter().copied().eq(0..map.len());
        segment::replay(
            &mut BufReader::with_capacity(256 << 10, self.rows.open()?),
            |mut row| {
                resource::check_cancel()?;
                if !same {
                    remap(&mut row, &map);
                }
                visit(&row)
            },
        )
    }
}

impl Group {
    fn inference(&self, width: usize, order: Option<&[usize]>) -> Result<Inference> {
        let mut inference = Inference::new(width);
        for part in &self.parts {
            let (mut map, hints): (Vec<usize>, Inference) =
                serde_json::from_reader(BufReader::new(part.hints.open()?))?;
            if let Some(order) = order {
                for col in &mut map {
                    *col = order[*col];
                }
            }
            inference.merge_mapped(&hints, &map)?;
        }
        Ok(inference)
    }
}

fn build(o: Build, mem: &resource::MemoryGuard) -> Result<bool> {
    let start = std::time::Instant::now();
    let cfg = config(&o.common)?;
    let s = scratch(&o.common, &cfg)?;
    let limit_mib = o.max_html_mib.or(cfg.max_html_mib).unwrap_or(32);
    ensure!(limit_mib > 0, "--max-html-mib must be positive");
    let limit = usize::try_from(
        limit_mib
            .checked_mul(1 << 20)
            .context("HTML limit too large")?,
    )?;
    let files = discover(&o.inputs, o.recursive, s.path())?;
    let paths = OutputPaths::validate(&o, &files)?;
    let mut issues = paths
        .issues
        .as_ref()
        .map(|p| {
            NamedTempFile::new_in(parent(p)).map(|inner| {
                BufWriter::new(VolumeWriter {
                    inner,
                    dir: parent(p).into(),
                    until: 0,
                })
            })
        })
        .transpose()?;
    let clean = paths
        .clean
        .as_ref()
        .map(|p| {
            tempfile::Builder::new()
                .prefix(".dataduck-export-")
                .tempdir_in(parent(p))
        })
        .transpose()?;
    if let Some(d) = &clean {
        File::create(d.path().join(".dataduck-export"))?;
    }
    if !o.quiet {
        eprintln!(
            "Scratch {} ({} GiB limit); {} input files",
            s.path().display(),
            o.common.temp_limit_gib.or(cfg.temp_limit_gib).unwrap_or(24),
            files.len()
        );
    }
    let data = ingest(files, &cfg, &s, o.quiet, &mut issues)?;
    let mut profiles = Vec::new();
    let mut sections = Vec::new();
    for (gi, g) in data.groups.into_iter().enumerate() {
        let headers: Vec<String> = serde_json::from_reader(BufReader::new(g.headers.open()?))?;
        s.check()?;
        if !o.quiet {
            eprintln!(
                "Profiling {} ({} columns, {} sources)",
                g.name,
                headers.len(),
                g.parts.len()
            );
        }
        let inf = g.inference(headers.len(), None)?;
        let mut profiler = Profiler::new(&headers, inf, &s)?;
        let mut csv = if let Some(d) = &clean {
            let f = File::create(d.path().join(format!("table-{:04}.csv", gi + 1)))?;
            let mut wr = csv::WriterBuilder::new()
                .buffer_capacity(256 << 10)
                .from_writer(VolumeWriter {
                    inner: f,
                    dir: d.path().into(),
                    until: 0,
                });
            wr.write_record(&headers)?;
            Some(wr)
        } else {
            None
        };
        for part in g.parts {
            part.replay(None, |r| {
                profiler.observe(r, &part.source)?;
                if let Some(w) = &mut csv {
                    write_csv(w, r)?;
                }
                Ok(())
            })?;
        }
        if let Some(mut w) = csv {
            w.flush()?;
            w.get_ref().inner.sync_all()?;
        }
        let p = profiler.finish()?;
        let mut file = BufWriter::with_capacity(128 << 10, s.create()?);
        serde_json::to_writer(&mut file, &p)?;
        file.flush()?;
        let file = file.into_inner().map_err(|e| e.into_error())?.park()?;
        sections.push(report::TableSection {
            name: g.name,
            columns: headers.len(),
            clean_file: clean.as_ref().map(|_| format!("table-{:04}.csv", gi + 1)),
            rows: p.rows,
            profile: file.path().into(),
        });
        profiles.push(file);
    }
    let partial = data.sources.partial;
    let mut html = NamedTempFile::new_in(parent(&paths.html))?;
    {
        let mut out = BufWriter::new(VolumeWriter {
            inner: html.as_file_mut(),
            dir: parent(&paths.html).into(),
            until: 0,
        });
        report::write_report(&mut out, &data.sources, &sections, limit)?;
        out.flush()?;
    }
    html.as_file().sync_all()?;
    s.check()?;
    let issues = issues
        .map(|mut w| -> Result<_> {
            w.flush()?;
            w.get_ref().inner.as_file().sync_all()?;
            Ok(w.into_inner().map_err(|e| e.into_error())?.inner)
        })
        .transpose()?;
    publish(&paths, html, issues, clean, o.overwrite)?;
    if !o.quiet {
        eprintln!(
            "{} accepted rows; peak scratch {} bytes; {}; {:.2}s",
            data.sources.rows,
            s.peak(),
            mem.stats(),
            start.elapsed().as_secs_f64()
        );
    }
    eprintln!(
        "{}{}",
        paths.html.display(),
        if partial { " (partial report)" } else { "" }
    );
    Ok(partial)
}
fn publish(
    paths: &OutputPaths,
    html: NamedTempFile,
    issues: Option<NamedTempFile>,
    clean: Option<TempDir>,
    overwrite: bool,
) -> Result<()> {
    let mut owned = Vec::new();
    let mut owned_dir = None;
    let res = (|| -> Result<()> {
        if let (Some(stage), Some(dest)) = (clean, &paths.clean) {
            fs::create_dir(dest)?;
            owned_dir = Some(dest.clone());
            for ent in stage.path().read_dir()? {
                let e = ent?;
                let dst = dest.join(e.file_name());
                fs::hard_link(e.path(), &dst).or_else(|_| {
                    let mut target = fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&dst)?;
                    let result = io::copy(&mut File::open(e.path())?, &mut target)
                        .and_then(|_| target.sync_all());
                    if result.is_err() {
                        drop(target);
                        let _ = fs::remove_file(&dst);
                    }
                    result
                })?;
                owned.push(dst);
            }
        }
        if let (Some(stage), Some(dest)) = (issues, &paths.issues) {
            stage.persist_noclobber(dest)?;
            owned.push(dest.clone());
        }
        resource::check_cancel()?;
        if overwrite {
            html.persist(&paths.html)?;
        } else {
            html.persist_noclobber(&paths.html)?;
        }
        Ok(())
    })();
    if res.is_err() {
        for p in owned {
            let _ = fs::remove_file(p);
        }
        if let Some(d) = owned_dir {
            let _ = fs::remove_dir(d);
        }
    }
    res
}
fn install_cancel_handler() -> Result<()> {
    ctrlc::set_handler(|| resource::CANCELLED.store(true, std::sync::atomic::Ordering::Relaxed))?;
    Ok(())
}

fn run_command(cli: Cli, guard: &resource::MemoryGuard) -> Result<bool> {
    match cli.command {
        Command::Build(o) => build(o, guard),
        Command::PowerBi(o) => powerbi::run(o, guard),
        Command::Inspect(o) => {
            inspect(o)?;
            Ok(false)
        }
    }
}

fn command_exit_code(result: Result<bool>) -> std::process::ExitCode {
    match result {
        Ok(false) => 0.into(),
        Ok(true) => 2.into(),
        Err(e) => {
            eprintln!("dataduck: {e:#}");
            if e.is::<InvalidSettings>() {
                64.into()
            } else {
                1.into()
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    if prompt::should_open() {
        return command_exit_code(prompt::run().map(|()| false));
    }
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        let code = if e.use_stderr() { 64 } else { 0 };
        let _ = e.print();
        std::process::exit(code)
    });
    command_exit_code((|| {
        let guard = resource::MemoryGuard::install()?;
        install_cancel_handler()?;
        run_command(cli, &guard)
    })())
}
