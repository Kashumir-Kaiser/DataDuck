//! Streaming CSV partitions consumed by the bundled Power BI model.
use super::{AxisSpec, Table};
use crate::model::{Kind, Row, Source};
use crate::profile::{export_value, Column, Type};
use crate::VolumeWriter;
use anyhow::{ensure, Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub const MISSING: i32 = i32::MIN;

pub fn fact_file(id: &str) -> String {
    format!("fact-{id}.csv")
}
pub fn axis_file(id: &str, field: usize) -> String {
    format!("axis-{id}-{:04}.csv", field + 1)
}
pub fn field_header(field: usize) -> String {
    format!("F{:04}", field + 1)
}
pub fn axis_header(field: usize) -> String {
    format!("A{:04}", field + 1)
}

type Partition = csv::Writer<VolumeWriter<File>>;

fn output(dir: &Path, name: &str) -> Result<VolumeWriter<File>> {
    let path = dir.join(name);
    let inner = File::create_new(&path).with_context(|| format!("create {}", path.display()))?;
    Ok(VolumeWriter {
        inner,
        dir: dir.to_owned(),
        until: 0,
    })
}

fn partition(dir: &Path, name: &str, headers: &[&str]) -> Result<Partition> {
    let mut w = csv::WriterBuilder::new()
        .has_headers(false)
        .buffer_capacity(256 << 10)
        .from_writer(output(dir, name)?);
    w.write_record(headers)?;
    Ok(w)
}

fn sync(mut w: Partition) -> Result<()> {
    w.flush()?;
    w.get_ref().inner.sync_all()?;
    Ok(())
}

/// Desktop's import limit is on UTF-16 length. Skip scanning ordinary short cells.
fn fits_text(value: &str) -> bool {
    value.len() <= 32_000 || value.encode_utf16().take(32_001).count() <= 32_000
}

pub struct SnapshotWriter {
    profile: Partition,
    missing: Partition,
    sources: Partition,
    decisions: BufWriter<VolumeWriter<File>>,
}

impl SnapshotWriter {
    pub fn new(dir: &Path) -> Result<Self> {
        Ok(Self {
            profile: partition(
                dir,
                "profile.csv",
                &[
                    "Table",
                    "Field",
                    "Kind",
                    "Rows",
                    "Valid",
                    "Missing",
                    "Distinct",
                    "Minimum",
                    "Maximum",
                    "Mean",
                    "Missing reasons",
                ],
            )?,
            missing: partition(dir, "missing.csv", &["Table", "Field", "Reason", "Count"])?,
            sources: partition(
                dir,
                "sources.csv",
                &[
                    "Source", "Sheet", "Table", "Status", "Rows", "Rejected", "Message",
                ],
            )?,
            decisions: BufWriter::with_capacity(128 << 10, output(dir, "chart-decisions.ndjson")?),
        })
    }
    pub fn profile(&mut self, table: &str, c: &Column, rows: u64) -> Result<()> {
        ensure!(
            fits_text(&c.name) && fits_text(table),
            "Power BI profile for table {table} has a header longer than 32,000 UTF-16 units"
        );
        let mut reasons = Vec::with_capacity(c.missing_reasons.len());
        for (reason, count) in &c.missing_reasons {
            self.missing.serialize((table, &c.name, reason, count))?;
            reasons.push(format!("{reason}: {count}"));
        }
        let reasons = reasons.join(", ");
        ensure!(
            fits_text(&reasons),
            "missing reasons exceed Power BI text limit for {} in {table}",
            c.name
        );
        // Pure text/boolean fields have no numeric extrema. A constant still has
        // one exact value worth displaying in the fixed export profile.
        let constant = c
            .top
            .first()
            .filter(|_| c.distinct == 1)
            .map(|v| v.value.as_str());
        ensure!(
            constant.is_none_or(fits_text),
            "constant value exceeds Power BI text limit for {} in {table}",
            c.name
        );
        self.profile.serialize((
            table,
            &c.name,
            c.kind,
            rows,
            c.valid,
            c.missing,
            c.distinct,
            c.min.as_deref().or(constant),
            c.max.as_deref().or(constant),
            c.mean,
            reasons,
        ))?;
        Ok(())
    }
    pub fn source(&mut self, s: &Source) -> Result<()> {
        ensure!(
            [
                Some(s.path.as_str()),
                s.sheet.as_deref(),
                s.table.as_deref(),
                Some(&s.status),
                Some(&s.message)
            ]
            .into_iter()
            .flatten()
            .all(fits_text),
            "a Power BI source audit text value exceeds 32,000 UTF-16 units"
        );
        self.sources.serialize((
            &s.path,
            s.sheet.as_deref(),
            s.table.as_deref(),
            &s.status,
            s.rows,
            s.rejected_rows,
            &s.message,
        ))?;
        Ok(())
    }
    pub fn decisions(&mut self) -> &mut impl Write {
        &mut self.decisions
    }
    pub fn finish(mut self) -> Result<()> {
        sync(self.profile)?;
        sync(self.missing)?;
        sync(self.sources)?;
        self.decisions.flush()?;
        self.decisions.get_ref().inner.sync_all()?;
        Ok(())
    }
}

/// Every key maps to exactly one label, including literal source values such as "Other".
/// Sort is separate from Key so reserved entries can appear at the end of an axis.
struct Member {
    key: i32,
    label: String,
    sort: i32,
    detail: String,
}

fn short_label(s: &str) -> String {
    let mut chars = s.chars();
    let label: String = chars.by_ref().take(38).collect();
    if chars.next().is_some() {
        format!("{label}…")
    } else {
        label
    }
}

fn distinguish_members(members: &mut [Member]) {
    // The default semantic model collation can fold case, trailing blanks and
    // Unicode variants. Uncertain collations get a numeric prefix on the axis;
    // the tooltip always carries the original, complete category spelling.
    let mut stems = HashSet::from(["other".to_owned(), "missing".to_owned()]);
    // Consider the reserved labels on the first export even if their rows
    // appear for the first time in a later snapshot; saved filters stay valid.
    let need_keys = members
        .iter()
        .filter(|m| m.key > 0)
        .any(|m| !m.label.is_ascii() || !stems.insert(m.label.trim_end().to_ascii_lowercase()));
    if need_keys {
        for m in members {
            m.label = format!("[{}] {}", m.key, m.label);
        }
    }
}

struct LiveAxis<'a> {
    field: usize,
    spec: &'a AxisSpec,
    lookup: HashMap<&'a str, i32>,
    // Distinct keys on every supported dimension are bounded by the spec;
    // a year is separately validated to the fixed four-digit domain.
    used: BTreeSet<i32>,
    current: i32,
}

impl<'a> LiveAxis<'a> {
    fn new(field: usize, spec: &'a AxisSpec) -> Result<Self> {
        let mut lookup = HashMap::new();
        if let AxisSpec::Categories(values) | AxisSpec::Discrete(values) = spec {
            for (i, value) in values.iter().enumerate() {
                ensure!(
                    fits_text(value),
                    "Power BI axis field {} exceeds 32,000 UTF-16 units",
                    field + 1
                );
                ensure!(
                    lookup
                        .insert(value.as_str(), i32::try_from(i + 1)?)
                        .is_none(),
                    "duplicate axis member in field {}",
                    field + 1
                );
            }
        }
        Ok(Self {
            field,
            spec,
            lookup,
            used: BTreeSet::new(),
            current: MISSING,
        })
    }

    fn observe(&mut self, value: Option<&str>) -> Result<()> {
        self.current = match (self.spec, value) {
            (_, None) => MISSING,
            (AxisSpec::Categories(_) | AxisSpec::Discrete(_), Some(v)) => {
                self.lookup.get(v).copied().unwrap_or(0)
            }
            (AxisSpec::Years, Some(v)) => {
                let year: i32 = v
                    .parse()
                    .context("saved year axis requires integral four-digit years")?;
                ensure!(
                    (1000..=9999).contains(&year),
                    "year outside saved four-digit year axis"
                );
                year
            }
            (AxisSpec::Histogram(spec), Some(v)) => spec.key(v)?,
            (AxisSpec::Calendar(spec), Some(v)) => spec.key(v)?,
        };
        self.used.insert(self.current);
        Ok(())
    }

    fn write(self, dir: &Path, table: &str) -> Result<()> {
        let mut members = Vec::new();
        match self.spec {
            AxisSpec::Categories(values) | AxisSpec::Discrete(values) => {
                for (i, value) in values.iter().enumerate() {
                    let key = i32::try_from(i + 1)?;
                    members.push(Member {
                        key,
                        sort: key,
                        label: short_label(value),
                        detail: value.clone(),
                    });
                }
                if self.used.contains(&0) {
                    members.push(Member {
                        key: 0,
                        sort: i32::MAX - 1,
                        label: "Other".to_owned(),
                        detail: "Other values outside the initial category selection".to_owned(),
                    });
                }
            }
            AxisSpec::Years => {
                for &year in self.used.range(1000..=9999) {
                    let detail = year.to_string();
                    members.push(Member {
                        key: year,
                        sort: year,
                        label: detail.clone(),
                        detail,
                    });
                }
            }
            AxisSpec::Histogram(h) => {
                let rows = h.rows()?;
                let last = rows.last().context("empty histogram axis")?.0;
                for (key, label, detail) in rows {
                    if (key == 0 || key == last) && !self.used.contains(&key) {
                        continue;
                    }
                    members.push(Member {
                        key,
                        sort: key,
                        label,
                        detail,
                    });
                }
            }
            AxisSpec::Calendar(c) => {
                for (key, label, detail) in c.rows()? {
                    members.push(Member {
                        key,
                        sort: key,
                        label,
                        detail,
                    });
                }
            }
        }
        if self.used.contains(&MISSING) {
            members.push(Member {
                key: MISSING,
                sort: i32::MAX,
                label: "Missing".to_owned(),
                detail: "Missing or absent value".to_owned(),
            });
        }
        if matches!(self.spec, AxisSpec::Categories(_) | AxisSpec::Discrete(_)) {
            distinguish_members(&mut members);
        }
        let keys: BTreeSet<_> = members.iter().map(|m| m.key).collect();
        ensure!(
            keys.len() == members.len() && self.used.is_subset(&keys),
            "Power BI axis on {} field {} contains an unbound or duplicate key",
            table,
            self.field + 1
        );
        let mut out = partition(
            dir,
            &axis_file(table, self.field),
            &["Key", "Label", "Sort", "Detail"],
        )?;
        for m in members {
            out.serialize((m.key, m.label, m.sort, m.detail))?;
        }
        sync(out)
    }
}

struct PairSupport {
    axis: usize,
    measure_field: usize,
    rows: u64,
    keys: [i32; 3],
    groups: usize,
}

impl PairSupport {
    fn observe(&mut self, key: i32, row: &Row) {
        if key == MISSING {
            return;
        }
        if let Ok(i) = row
            .cells
            .binary_search_by_key(&self.measure_field, |c| c.col)
        {
            if row.cells[i].kind != Kind::Missing {
                self.rows += 1;
                if self.groups < self.keys.len() && !self.keys[..self.groups].contains(&key) {
                    self.keys[self.groups] = key;
                    self.groups += 1;
                }
            }
        }
    }
    fn result(self) -> (u64, usize) {
        (self.rows, self.groups)
    }
}

pub struct FactWriter<'a> {
    dir: PathBuf,
    id: String,
    kinds: Vec<Type>,
    // In a wide table this is one small index per source field. Bound the
    // lookups to the shortlisted axes; do not scan them for every source cell.
    axis_by_field: Vec<Option<usize>>,
    axes: Vec<LiveAxis<'a>>,
    pairs: Vec<PairSupport>,
    out: Partition,
}

impl<'a> FactWriter<'a> {
    pub fn new(dir: &Path, table: &'a Table, pairs: &[(usize, usize)]) -> Result<Self> {
        let axes = table
            .axes
            .iter()
            .map(|a| LiveAxis::new(a.field, &a.spec))
            .collect::<Result<_>>()?;
        let kinds = table
            .fields
            .iter()
            .map(|f| {
                if f.kind == Type::Empty {
                    Type::Text
                } else {
                    f.kind
                }
            })
            .collect();
        Self::open(dir, &table.id, kinds, axes, pairs)
    }
    pub fn plain(dir: &Path, id: &str, kinds: &[Type]) -> Result<Self> {
        Self::open(dir, id, kinds.to_vec(), vec![], &[])
    }
    fn open(
        dir: &Path,
        id: &str,
        kinds: Vec<Type>,
        axes: Vec<LiveAxis<'a>>,
        pairs: &[(usize, usize)],
    ) -> Result<Self> {
        let mut axis_by_field = vec![None; kinds.len()];
        for (i, axis) in axes.iter().enumerate() {
            let slot = axis_by_field
                .get_mut(axis.field)
                .context("invalid axis field")?;
            ensure!(slot.replace(i).is_none(), "duplicate axis field");
        }
        let pairs = pairs
            .iter()
            .map(|&(field, measure_field)| {
                ensure!(measure_field < kinds.len(), "invalid measure field");
                let axis = axis_by_field
                    .get(field)
                    .copied()
                    .flatten()
                    .context("pair has no fact axis")?;
                Ok(PairSupport {
                    axis,
                    measure_field,
                    rows: 0,
                    keys: [0; 3],
                    groups: 0,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let headers = (0..kinds.len())
            .map(field_header)
            .chain(["Source".to_owned(), "Row".to_owned()])
            .chain(axes.iter().map(|a| axis_header(a.field)))
            .collect::<Vec<_>>();
        let out = partition(
            dir,
            &fact_file(id),
            &headers.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
        Ok(Self {
            dir: dir.to_owned(),
            id: id.to_owned(),
            kinds,
            axis_by_field,
            axes,
            pairs,
            out,
        })
    }

    pub fn row(&mut self, row: &Row, source: &str) -> Result<()> {
        ensure!(
            row.width == self.kinds.len(),
            "Power BI fact width mismatch in {source} row {}",
            row.number
        );
        let mut cells = row.cells.iter().peekable();
        for (i, &kind) in self.kinds.iter().enumerate() {
            let value = if cells.peek().is_some_and(|c| c.col == i) {
                let c = cells.next().unwrap();
                if c.kind == Kind::Missing {
                    None
                } else {
                    let v = export_value(c, kind)
                        .with_context(|| format!("{source} row {}, field {}", row.number, i + 1))?;
                    ensure!(fits_text(&v), "{source} row {}, field {} exceeds the Power BI limit of 32,000 UTF-16 units",row.number,i+1);
                    Some(v)
                }
            } else {
                None
            };
            self.out
                .write_field(value.as_deref().unwrap_or("").as_bytes())?;
            if let Some(idx) = self.axis_by_field[i] {
                self.axes[idx].observe(value.as_deref()).with_context(|| {
                    format!("axis in {source} row {}, field {}", row.number, i + 1)
                })?;
            }
        }
        ensure!(
            cells.next().is_none(),
            "invalid cell order in {source} row {}",
            row.number
        );
        ensure!(fits_text(source), "source path exceeds 32,000 UTF-16 units");
        self.out.write_field(source.as_bytes())?;
        let mut digits = itoa::Buffer::new();
        self.out.write_field(digits.format(row.number))?;
        for p in &mut self.pairs {
            p.observe(self.axes[p.axis].current, row);
        }
        for axis in &self.axes {
            self.out.write_field(digits.format(axis.current))?;
        }
        self.out.write_record(std::iter::empty::<&str>())?;
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<(u64, usize)>> {
        sync(self.out)?;
        for a in self.axes {
            a.write(&self.dir, &self.id)?;
        }
        Ok(self.pairs.into_iter().map(PairSupport::result).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cell;
    use crate::powerbi::{Axis, Field};
    use crate::profile::ImportType;

    fn table() -> Table {
        Table {
            id: "table-test".into(),
            name: "Cars".into(),
            fields: vec![
                Field {
                    name: "Make".into(),
                    key: "make".into(),
                    kind: Type::Text,
                    import: ImportType::Text,
                    utc: false,
                },
                Field {
                    name: "Price".into(),
                    key: "price".into(),
                    kind: Type::Decimal,
                    import: ImportType::Fixed,
                    utc: false,
                },
            ],
            axes: vec![Axis {
                field: 0,
                spec: AxisSpec::Categories(vec!["Ford".into(), "Other".into()]),
            }],
            charts: vec![],
        }
    }

    fn row(number: u64, make: Option<&str>, price: Option<&str>) -> Row {
        let mut r = Row {
            number,
            width: 2,
            ..Default::default()
        };
        if let Some(value) = make {
            r.cells.push(Cell {
                col: 0,
                value: value.into(),
                kind: Kind::Text,
            });
        }
        if let Some(value) = price {
            r.cells.push(Cell {
                col: 1,
                value: value.into(),
                kind: Kind::Number,
            });
        }
        r
    }

    fn records(path: impl AsRef<Path>) -> Result<Vec<Vec<String>>> {
        csv::Reader::from_path(path)?
            .records()
            .map(|r| Ok(r?.iter().map(str::to_owned).collect()))
            .collect()
    }

    #[test]
    fn saved_labels_and_measure_support_survive_new_categories() -> Result<()> {
        let project = table();
        let first = tempfile::tempdir()?;
        let mut initial = FactWriter::new(first.path(), &project, &[(0, 1)])?;
        initial.row(&row(2, Some("Ford"), Some("1.00")), "Cars")?;
        initial.row(&row(3, Some("Other"), Some("4.50")), "Cars")?;
        assert_eq!(initial.finish()?, [(2, 2)]);
        let path = axis_file(&project.id, 0);
        let old = records(first.path().join(&path))?;

        let next = tempfile::tempdir()?;
        let mut update = FactWriter::new(next.path(), &project, &[(0, 1)])?;
        update.row(&row(2, Some("Ford"), Some("1.00")), "Cars")?;
        update.row(&row(3, Some("Other"), None), "Cars")?;
        update.row(&row(4, None, Some("4.50")), "Cars")?;
        update.row(&row(5, Some("Brand,\nnew"), Some("7")), "Cars")?;
        assert_eq!(update.finish()?, [(2, 2)]);
        let new = records(next.path().join(&path))?;
        assert_eq!(&new[..2], &old);
        assert_eq!(new[2][0], "0");
        assert_ne!(new[2][1], new[1][1]);
        assert_eq!(new[3][0], MISSING.to_string());
        let rows = records(next.path().join(fact_file(&project.id)))?;
        assert_eq!(rows[3], ["Brand,\nnew", "7", "Cars", "5", "0"]);
        assert_eq!(rows[2][0], "");
        assert_eq!(rows[2][4], MISSING.to_string());
        Ok(())
    }

    #[test]
    fn cap_counts_utf16_code_units_and_plain_rows_preserve_quoting() -> Result<()> {
        assert!(fits_text(&"🐥".repeat(16_000)));
        assert!(!fits_text(&"🐥".repeat(16_001)));
        let dir = tempfile::tempdir()?;
        let mut facts = FactWriter::plain(dir.path(), "X", &[Type::Text, Type::Decimal])?;
        facts.row(
            &row(7, Some("A \"quoted\"\nrow"), Some("1.00")),
            "File / Sheet",
        )?;
        facts.finish()?;
        let rows = records(dir.path().join("fact-X.csv"))?;
        assert_eq!(rows[0], ["A \"quoted\"\nrow", "1", "File / Sheet", "7"]);
        Ok(())
    }

    #[test]
    fn snapshots_write_exact_profile_and_sources_as_regular_csv() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut snapshots = SnapshotWriter::new(dir.path())?;
        let col: Column = serde_json::from_value(serde_json::json!({
            "name":"Make","kind":"text","valid":2,"missing":1,"distinct":2,"top":[],
            "other":2,"min":null,"max":null,"mean":null,"bins":[], "missing_reasons":{"absent":1}
        }))?;
        snapshots.profile("Cars", &col, 3)?;
        snapshots.source(&Source {
            path: "Cars.xlsx".into(),
            sheet: Some("Cars".into()),
            status: "complete".into(),
            rows: 3,
            table: Some("Cars".into()),
            ..Default::default()
        })?;
        writeln!(snapshots.decisions(), "{{}}")?;
        snapshots.finish()?;
        assert_eq!(
            records(dir.path().join("profile.csv"))?[0],
            [
                "Cars",
                "Make",
                "text",
                "3",
                "2",
                "1",
                "2",
                "",
                "",
                "",
                "absent: 1"
            ]
        );
        assert_eq!(
            records(dir.path().join("sources.csv"))?[0],
            ["Cars.xlsx", "Cars", "Cars", "complete", "3", "0", ""]
        );
        assert_eq!(
            records(dir.path().join("missing.csv"))?[0],
            ["Cars", "Make", "absent", "1"]
        );
        Ok(())
    }
}
