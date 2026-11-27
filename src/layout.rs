//! Source recipes, conservative header detection and value-preserving row cleanup.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::model::{Cell, Issue, Kind, Row};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

const MAX_COLUMNS: usize = 16_384;
const MAX_CONFIG_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub temp_dir: Option<PathBuf>,
    pub temp_limit_gib: Option<u64>,
    pub max_html_mib: Option<u64>,
    #[serde(rename = "source")]
    pub sources: Vec<SourceRule>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut input = Vec::new();
        std::fs::File::open(path)
            .with_context(|| format!("open recipe {}", path.display()))?
            .take(MAX_CONFIG_BYTES as u64 + 1)
            .read_to_end(&mut input)?;
        ensure!(input.len() <= MAX_CONFIG_BYTES, "recipe exceeds 2 MiB");
        let text = std::str::from_utf8(&input).context("recipe must be UTF-8")?;
        Self::parse(text).with_context(|| format!("recipe {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut config: Config = toml::from_str(text)?;
        ensure!(config.version == 1, "expected recipe version = 1");
        ensure!(
            config.temp_limit_gib != Some(0),
            "temp_limit_gib must be positive"
        );
        ensure!(
            config.max_html_mib != Some(0),
            "max_html_mib must be positive"
        );
        for (index, rule) in config.sources.iter_mut().enumerate() {
            rule.validate()
                .with_context(|| format!("source rule {}", index + 1))?;
        }
        Ok(config)
    }

    pub fn rule_for(
        &self,
        relative_path: &str,
        sheet: Option<&str>,
    ) -> Result<Option<&SourceRule>> {
        let mut found = None;
        let name = relative_path.replace('\\', "/");
        for rule in &self.sources {
            let matches_path = rule.matcher.as_ref().is_some_and(|m| m.is_match(&name));
            let matches_sheet = rule.sheet.as_deref().map_or(true, |s| Some(s) == sheet);
            if matches_path && matches_sheet {
                ensure!(
                    found.is_none(),
                    "multiple recipes match {name} {}. Use mutually exclusive path/sheet rules",
                    sheet.unwrap_or("")
                );
                found = Some(rule);
            }
        }
        Ok(found)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceRule {
    pub path: String,
    pub sheet: Option<String>,
    pub header_rows: Vec<u64>,
    pub headerless: bool,
    pub data_start_row: Option<u64>,
    pub data_end_row: Option<u64>,
    /// Inclusive one-based column indices, e.g. [2, 6] for B:F.
    pub column_range: Option<[usize; 2]>,
    pub names: Option<Vec<String>>,
    pub skip_repeated_headers: bool,
    pub delimiter: Option<String>,
    pub encoding: Option<String>,
    pub columns: BTreeMap<String, ColumnRule>,
    #[serde(skip)]
    matcher: Option<GlobMatcher>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    Text,
    Number,
    Integer,
    Boolean,
    Date,
    Datetime,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColumnRule {
    #[serde(rename = "type")]
    pub kind: Option<ColumnType>,
    pub date_format: Option<String>,
    pub null_tokens: Vec<String>,
}

impl SourceRule {
    fn validate(&mut self) -> Result<()> {
        ensure!(!self.path.trim().is_empty(), "path is required");
        let glob = GlobBuilder::new(&self.path.replace('\\', "/"))
            .literal_separator(true)
            .case_insensitive(true)
            .build()
            .context("invalid path glob")?;
        self.matcher = Some(glob.compile_matcher());
        if !self.header_rows.is_empty() {
            let rows = &self.header_rows;
            ensure!(!self.headerless, "header_rows conflicts with headerless");
            ensure!(
                rows[0] >= 1 && rows.windows(2).all(|r| r[0] < r[1]),
                "header_rows must contain strictly increasing positive coordinates"
            );
            ensure!(rows.len() <= 32, "at most 32 header rows are supported");
            if let Some(start) = self.data_start_row {
                ensure!(
                    start > *rows.last().unwrap(),
                    "data_start_row must follow the header"
                );
            }
            if let Some(end) = self.data_end_row {
                ensure!(
                    end >= *rows.last().unwrap(),
                    "data_end_row precedes the header"
                );
            }
        }
        ensure!(
            self.data_start_row != Some(0) && self.data_end_row != Some(0),
            "row coordinates start at 1"
        );
        if let (Some(start), Some(end)) = (self.data_start_row, self.data_end_row) {
            ensure!(start <= end, "data_end_row precedes data_start_row");
        }
        if let Some([first, last]) = self.column_range {
            ensure!(
                first > 0 && first <= last && last <= MAX_COLUMNS,
                "column_range must be an inclusive pair between 1 and {MAX_COLUMNS}"
            );
        }
        if let Some(names) = &self.names {
            ensure!(
                !names.is_empty() && names.len() <= MAX_COLUMNS,
                "names must contain between 1 and {MAX_COLUMNS} column names"
            );
            validate_names(names)?;
            if let Some([first, last]) = self.column_range {
                ensure!(
                    last - first + 1 == names.len(),
                    "names length differs from column_range width"
                );
            }
        }
        if let Some(delimiter) = &self.delimiter {
            ensure!(
                delimiter.len() == 1
                    && delimiter.is_ascii()
                    && !matches!(delimiter.as_bytes()[0], 0 | b'"' | b'\r' | b'\n'),
                "delimiter must be one ASCII character other than quote, NUL or newline"
            );
        }
        let mut rule_names = BTreeSet::new();
        for (name, column) in &self.columns {
            let key = header_key(name);
            ensure!(
                !key.is_empty() && rule_names.insert(key),
                "empty or duplicate normalized column rule {name:?}"
            );
            if let Some(format) = &column.date_format {
                ensure!(
                    !format.is_empty()
                        && !chrono::format::StrftimeItems::new(format)
                            .any(|i| i == chrono::format::Item::Error),
                    "invalid date_format for {name}"
                );
                ensure!(
                    matches!(
                        column.kind,
                        None | Some(ColumnType::Date | ColumnType::Datetime)
                    ),
                    "date_format requires a date or datetime column for {name}"
                );
            }
        }
        Ok(())
    }
}

pub fn header_key(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for word in name.split_whitespace() {
        if !result.is_empty() {
            result.push(' ');
        }
        result.extend(word.chars().flat_map(char::to_lowercase));
    }
    result
}

fn validate_names(names: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (index, name) in names.iter().enumerate() {
        let key = header_key(name);
        ensure!(
            !key.is_empty(),
            "column {} has an empty header; configure names",
            index + 1
        );
        ensure!(
            seen.insert(key),
            "duplicate normalized header {name:?}; use explicit names"
        );
    }
    Ok(())
}

pub struct Layout {
    pub headers: Vec<String>,
    pub description: String,
    first_col: usize,
    header_coords: Vec<u64>,
    data_start: u64,
    /// Preserve original source labels for exact repeated-header detection after renaming.
    repeated: Vec<String>,
    /// Header indices mapped once, irrespective of the number of rows replayed.
    rules: BTreeMap<usize, ColumnRule>,
}

fn populated(cell: &Cell) -> bool {
    !matches!(cell.kind, Kind::Missing) && !cell.value.trim().is_empty()
}

fn observed_end(row: &Row) -> usize {
    row.cells
        .iter()
        .filter(|c| populated(c))
        .map(|c| c.col + 1)
        .max()
        .unwrap_or(0)
}

fn row_width(row: &Row) -> usize {
    if row.sparse {
        observed_end(row)
    } else {
        row.width
    }
}

fn selected_values(row: &Row, first: usize, end: usize) -> Vec<&str> {
    let mut values = vec![""; end - first];
    for cell in &row.cells {
        if (first..end).contains(&cell.col) && populated(cell) {
            values[cell.col - first] = &cell.value;
        }
    }
    values
}

fn header_label(cell: &Cell) -> bool {
    if !populated(cell) || matches!(cell.kind, Kind::Bool | Kind::Number | Kind::Date) {
        return false;
    }
    let s = cell.value.trim();
    !(s.eq_ignore_ascii_case("true")
        || s.eq_ignore_ascii_case("false")
        || s.parse::<f64>().is_ok()
        || NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
        || DateTime::parse_from_rfc3339(s).is_ok())
}

fn header_span(row: &Row, range: Option<[usize; 2]>) -> Result<(usize, usize)> {
    let (first, end) = range.map_or((0, observed_end(row)), |[f, l]| (f - 1, l));
    ensure!(
        first < end && end <= MAX_COLUMNS,
        "no usable header cells in the selected range at row {}",
        row.number
    );
    Ok((first, end))
}

fn text_header(row: &Row, first: usize, end: usize) -> bool {
    let mut seen = BTreeSet::new();
    for cell in row.cells.iter().filter(|c| (first..end).contains(&c.col)) {
        if !header_label(cell) || !seen.insert(header_key(&cell.value)) {
            return false;
        }
    }
    seen.len() == end - first
}

fn compatible(row: &Row, first: usize, end: usize, fixed_range: bool) -> bool {
    if fixed_range {
        return row
            .cells
            .iter()
            .any(|c| (first..end).contains(&c.col) && populated(c));
    }
    if row.sparse {
        let width = observed_end(row);
        width > first && width <= end
    } else {
        row.width == end || (row.width > end && observed_end(row) == end)
    }
}

/// The caller probes up to 32 nonempty rows or 2 MiB for auto detection and supplies
/// all explicitly selected header rows for a recipe, even when beyond the probe.
pub fn detect(probe: &[Row], rule: &SourceRule) -> Result<Layout> {
    let (first_col, end_col, coords, originals, description) = if rule.headerless {
        let first = rule.column_range.map_or(0, |[f, _]| f - 1);
        let end = rule
            .column_range
            .map(|[_, l]| l)
            .or_else(|| rule.names.as_ref().map(|n| first + n.len()))
            .unwrap_or_else(|| {
                probe
                    .iter()
                    .filter(|r| {
                        r.number >= rule.data_start_row.unwrap_or(1)
                            && r.number <= rule.data_end_row.unwrap_or(u64::MAX)
                    })
                    .map(row_width)
                    .max()
                    .unwrap_or(0)
            });
        ensure!(
            end > first && end <= MAX_COLUMNS,
            "headerless source has no detectable width; configure names or column_range"
        );
        (
            first,
            end,
            vec![],
            vec![],
            "Headerless layout from recipe".into(),
        )
    } else if !rule.header_rows.is_empty() {
        let mut parts = Vec::with_capacity(rule.header_rows.len());
        for &coord in &rule.header_rows {
            parts.push(probe.iter().find(|r| r.number == coord).with_context(|| {
                format!(
                    "configured header row {coord} was absent; inspect original source coordinates"
                )
            })?);
        }
        let first = rule.column_range.map_or(0, |[f, _]| f - 1);
        let end = rule
            .column_range
            .map(|[_, l]| l)
            .or_else(|| rule.names.as_ref().map(|n| first + n.len()))
            .unwrap_or_else(|| parts.iter().map(|r| observed_end(r)).max().unwrap_or(0));
        ensure!(
            end > first && end <= MAX_COLUMNS,
            "configured header has no usable width; configure names or column_range"
        );
        let mut labels = vec![String::new(); end - first];
        for row in parts {
            for (label, part) in labels.iter_mut().zip(selected_values(row, first, end)) {
                if !part.trim().is_empty() {
                    ensure!(
                        label.len().saturating_add(part.len()).saturating_add(3) <= 1024 * 1024,
                        "combined header exceeds 1 MiB"
                    );
                    if !label.is_empty() {
                        label.push_str(" / ");
                    }
                    label.push_str(part);
                }
            }
        }
        (
            first,
            end,
            rule.header_rows.clone(),
            labels,
            format!("Configured header rows {:?}", rule.header_rows),
        )
    } else {
        let nonempty: Vec<_> = probe
            .iter()
            .filter(|r| r.cells.iter().any(populated))
            .collect();
        ensure!(nonempty.len() >= 2, "cannot infer a header without a following data row; inspect this source and configure header_rows or headerless");
        let mut header_pos = 0;
        if nonempty.len() >= 3 && nonempty[0].cells.iter().filter(|c| populated(c)).count() == 1 {
            let (first, end) = header_span(nonempty[1], rule.column_range)?;
            if end - first > 1
                && text_header(nonempty[1], first, end)
                && nonempty[2..]
                    .iter()
                    .take(2)
                    .all(|r| compatible(r, first, end, rule.column_range.is_some()))
            {
                header_pos = 1;
            }
        }
        let head = nonempty[header_pos];
        let (first, end) = header_span(head, rule.column_range)?;
        let data = &nonempty[header_pos + 1..];
        ensure!(text_header(head, first, end) || rule.names.is_some(),
            "ambiguous labels at row {}; inspect this source and configure header_rows and names, or headerless", head.number);
        // A malformed or short first data row can still be audited if later rows confirm width.
        ensure!(data.iter().any(|r| compatible(r, first, end, rule.column_range.is_some())),
            "unconfirmed header width at row {}; inspect this source and configure header_rows or column_range", head.number);
        let all_text = data
            .iter()
            .flat_map(|r| &r.cells)
            .filter(|c| (first..end).contains(&c.col) && populated(c))
            .all(header_label);
        let mut note = format!("Inferred header at row {}", head.number);
        if header_pos > 0 {
            note.push_str(" after a title row");
        }
        if all_text {
            note.push_str(" (assumed first label row in an all-text table)");
        }
        let labels = selected_values(head, first, end)
            .into_iter()
            .map(str::to_owned)
            .collect();
        (first, end, vec![head.number], labels, note)
    };
    let headers = match &rule.names {
        Some(names) => {
            ensure!(
                names.len() == end_col - first_col,
                "configured names do not match the selected width"
            );
            names.clone()
        }
        None if rule.headerless => (first_col..end_col)
            .map(|i| format!("column_{}", i + 1))
            .collect(),
        None => originals.clone(),
    };
    validate_names(&headers)?;
    let positions: BTreeMap<_, _> = headers
        .iter()
        .enumerate()
        .map(|(i, n)| (header_key(n), i))
        .collect();
    let mut rules = BTreeMap::new();
    for (name, recipe) in &rule.columns {
        let index = positions
            .get(&header_key(name))
            .with_context(|| format!("recipe refers to unknown column {name:?}"))?;
        rules.insert(*index, recipe.clone());
    }
    let data_start = rule
        .data_start_row
        .unwrap_or(coords.last().map_or(1, |n| n.saturating_add(1)));
    Ok(Layout {
        headers,
        description,
        first_col,
        header_coords: coords,
        data_start,
        repeated: originals.iter().map(|s| header_key(s)).collect(),
        rules,
    })
}

fn row_issue(
    row: &Row,
    col: Option<usize>,
    code: &str,
    message: &str,
    excerpt: &str,
    loss: bool,
) -> Issue {
    let end = excerpt
        .char_indices()
        .map(|(n, _)| n)
        .take_while(|&n| n <= 256)
        .last()
        .unwrap_or(0);
    let text = if excerpt.len() > 256 {
        &excerpt[..end]
    } else {
        excerpt
    };
    Issue {
        row: row.number,
        line_start: row.line_start,
        line_end: row.line_end,
        col,
        code: code.into(),
        message: message.into(),
        excerpt: text.into(),
        loss,
    }
}

impl Layout {
    pub fn header_coordinates(&self) -> &[u64] {
        &self.header_coords
    }

    pub fn clean(
        &self,
        mut row: Row,
        rule: &SourceRule,
        issues: &mut impl FnMut(Issue) -> Result<()>,
    ) -> Result<Option<Row>> {
        let ignored = if self.header_coords.binary_search(&row.number).is_ok() {
            Some(("header", "header row"))
        } else if row.number < self.data_start {
            Some(("preamble", "before first data row"))
        } else if rule.data_end_row.is_some_and(|end| row.number > end) {
            Some(("outside_range", "after last configured data row"))
        } else {
            None
        };
        if let Some((code, why)) = ignored {
            issues(row_issue(&row, None, code, why, "", false))?;
            return Ok(None);
        }
        let end = self.first_col + self.headers.len();
        if rule.column_range.is_none() && row_width(&row) > end {
            if let Some(cell) = row.cells.iter().find(|c| {
                c.col >= end
                    && (populated(c) || matches!(c.kind, Kind::Missing) && !c.value.is_empty())
            }) {
                issues(row_issue(
                    &row,
                    Some(cell.col),
                    "extra_fields",
                    "row has populated cells beyond header",
                    &cell.value,
                    true,
                ))?;
                return Ok(None);
            }
            issues(row_issue(
                &row,
                None,
                "extra_empty_fields",
                "discarded surplus trailing empty fields",
                "",
                false,
            ))?;
        }
        let has_data = row.cells.iter().any(|c| {
            (self.first_col..end).contains(&c.col)
                && (populated(c) || matches!(c.kind, Kind::Missing) && !c.value.is_empty())
        });
        if !has_data {
            issues(row_issue(
                &row,
                None,
                "blank_row",
                "empty data row",
                "",
                false,
            ))?;
            return Ok(None);
        }
        if !row.sparse && row.width < end {
            issues(row_issue(
                &row,
                None,
                "short_row",
                "padded missing trailing fields",
                "",
                false,
            ))?;
        }
        if rule.skip_repeated_headers
            && !self.repeated.is_empty()
            && selected_values(&row, self.first_col, end)
                .iter()
                .zip(&self.repeated)
                .all(|(v, h)| header_key(v) == *h)
        {
            issues(row_issue(
                &row,
                None,
                "repeated_header",
                "complete header repeated in data",
                "",
                false,
            ))?;
            return Ok(None);
        }
        let mut cells = std::mem::take(&mut row.cells);
        cells.retain(|c| (self.first_col..end).contains(&c.col));
        for cell in &mut cells {
            let original_col = cell.col;
            cell.col -= self.first_col;
            if matches!(cell.kind, Kind::Missing) {
                continue;
            }
            if cell.value.trim().is_empty() {
                let reason = if cell.value.is_empty() {
                    "empty_cell"
                } else {
                    "whitespace_cell"
                };
                cell.value = reason.into();
                cell.kind = Kind::Missing;
                continue;
            }
            if let Some(recipe) = self.rules.get(&cell.col) {
                if recipe.null_tokens.iter().any(|token| token == &cell.value) {
                    cell.kind = Kind::Missing;
                    cell.value = "null_token".into();
                } else if let Err(why) = convert(cell, recipe) {
                    issues(row_issue(
                        &row,
                        Some(original_col),
                        "conversion_failed",
                        &why.to_string(),
                        &cell.value,
                        true,
                    ))?;
                    cell.kind = Kind::Missing;
                    cell.value = "conversion_failed".into();
                }
            }
        }
        row.cells = cells;
        row.width = self.headers.len();
        Ok(Some(row))
    }
}

fn convert(cell: &mut Cell, recipe: &ColumnRule) -> Result<()> {
    let kind = recipe
        .kind
        .or_else(|| recipe.date_format.as_ref().map(|_| ColumnType::Date));
    let value = cell.value.trim();
    match kind {
        None => {}
        Some(ColumnType::Text) => cell.kind = Kind::Text,
        Some(ColumnType::Number) => {
            ensure!(
                crate::profile::supports_number(value)
                    && value.parse::<f64>().is_ok_and(|v| v.is_finite()),
                "expected a finite decimal number"
            );
            cell.value = value.to_owned();
            cell.kind = Kind::Number;
        }
        Some(ColumnType::Integer) => {
            let digits = value.strip_prefix(['-', '+']).unwrap_or(value);
            ensure!(
                !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && crate::profile::supports_number(value)
                    && value.parse::<f64>().is_ok_and(|v| v.is_finite()),
                "expected a finite integer"
            );
            cell.value = value.to_owned();
            cell.kind = Kind::Number;
        }
        Some(ColumnType::Boolean) => {
            let v = if value.eq_ignore_ascii_case("true") {
                "true"
            } else if value.eq_ignore_ascii_case("false") {
                "false"
            } else {
                bail!("expected true or false")
            };
            cell.value = v.into();
            cell.kind = Kind::Bool;
        }
        Some(ColumnType::Date) => {
            let date = NaiveDate::parse_from_str(
                value,
                recipe.date_format.as_deref().unwrap_or("%Y-%m-%d"),
            )
            .context("date does not match the configured format")?;
            cell.value = date.format("%Y-%m-%d").to_string();
            cell.kind = Kind::Date;
        }
        Some(ColumnType::Datetime) => {
            let local_format = recipe
                .date_format
                .as_deref()
                .unwrap_or("%Y-%m-%dT%H:%M:%S%.f");
            let offset_dt = if recipe.date_format.is_some() {
                DateTime::parse_from_str(value, local_format)
            } else {
                DateTime::parse_from_rfc3339(value)
            };
            cell.value = if let Ok(dt) = offset_dt {
                dt.with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::AutoSi, true)
            } else {
                NaiveDateTime::parse_from_str(value, local_format)
                    .context("datetime does not match the configured format")?
                    .format("%Y-%m-%dT%H:%M:%S%.f")
                    .to_string()
            };
            cell.kind = Kind::Date;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(number: u64, values: &[&str]) -> Row {
        Row {
            number,
            line_start: number,
            line_end: number,
            sparse: false,
            width: values.len(),
            cells: values
                .iter()
                .enumerate()
                .map(|(col, v)| Cell {
                    col,
                    kind: Kind::Auto,
                    value: (*v).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn matches_recipes_and_rejects_conflicts() {
        let recipe = r#"version=1
            [[source]]
            path="monthly/*.xlsx"
            sheet="Transactions"
            header_rows=[4]
            [source.columns."Order ID"]
            type="text"
        "#;
        let config = Config::parse(recipe).unwrap();
        let rule = config
            .rule_for(r"Monthly\nov.XLSX", Some("Transactions"))
            .unwrap()
            .unwrap();
        assert_eq!(rule.header_rows, [4]);
        assert!(config
            .rule_for("monthly/sales.xlsx", Some("Notes"))
            .unwrap()
            .is_none());
        assert!(Config::parse(&format!("{recipe}\n[unknown]\nx=3")).is_err());
        let config =
            Config::parse(&format!("{recipe}\n[[source]]\npath='monthly/*.xlsx'")).unwrap();
        assert!(config
            .rule_for("monthly/sales.xlsx", Some("Transactions"))
            .is_err());
    }

    #[test]
    fn detects_title_and_preserves_literals() {
        let rule = SourceRule::default();
        let rows = [
            row(1, &["Monthly report"]),
            row(2, &[" Order ID ", "Region", "Amount"]),
            row(3, &["00123", " NA ", "0"]),
            row(4, &["00456", "-", "1"]),
        ];
        let layout = detect(&rows, &rule).unwrap();
        assert_eq!(layout.headers, [" Order ID ", "Region", "Amount"]);
        assert!(layout.description.contains("title"));
        let mut diagnostics = vec![];
        let mut callback = |i| {
            diagnostics.push(i);
            Ok(())
        };
        assert!(layout
            .clean(rows[0].clone(), &rule, &mut callback)
            .unwrap()
            .is_none());
        let cleaned = layout
            .clean(rows[2].clone(), &rule, &mut callback)
            .unwrap()
            .unwrap();
        assert_eq!(cleaned.cells[0].value, "00123");
        assert_eq!(cleaned.cells[1].value, " NA ");
        assert_eq!(cleaned.cells[2].value, "0");
        assert_eq!(diagnostics[0].code, "preamble");
    }

    #[test]
    fn normalizes_blanks_and_applies_exact_token_and_date_rules() {
        let mut rule = SourceRule::default();
        rule.header_rows = vec![3];
        rule.columns.insert(
            "region".into(),
            ColumnRule {
                null_tokens: vec!["NA".into()],
                ..Default::default()
            },
        );
        rule.columns.insert(
            "Date".into(),
            ColumnRule {
                date_format: Some("%d/%m/%Y".into()),
                ..Default::default()
            },
        );
        let layout = detect(&[row(3, &["Region", "Date", "Other"])], &rule).unwrap();
        let mut diag = vec![];
        let mut callback = |i| {
            diag.push(i);
            Ok(())
        };
        let good = layout
            .clean(row(4, &["NA", "30/01/2026", "  "]), &rule, &mut callback)
            .unwrap()
            .unwrap();
        assert_eq!(good.cells[0].value, "null_token");
        assert_eq!(good.cells[1].value, "2026-01-30");
        assert_eq!(good.cells[1].kind, Kind::Date);
        assert_eq!(good.cells[2].value, "whitespace_cell");
        let bad = layout
            .clean(row(5, &["na", "30/02/2026"]), &rule, &mut callback)
            .unwrap()
            .unwrap();
        assert_eq!(bad.cells[0].value, "na");
        assert_eq!(bad.cells[1].value, "conversion_failed");
        assert_eq!(bad.width, 3);
        assert!(diag.iter().any(|i| i.code == "short_row" && !i.loss));
        assert!(diag
            .iter()
            .any(|i| i.code == "conversion_failed" && i.loss && i.col == Some(1)));
    }

    #[test]
    fn rejects_data_outside_unrestricted_header_but_honors_configured_projection() {
        let mut rule = SourceRule::default();
        rule.header_rows = vec![10];
        let headers = [row(10, &["A", "B"])];
        let layout = detect(&headers, &rule).unwrap();
        let mut diag = vec![];
        let mut callback = |i| {
            diag.push(i);
            Ok(())
        };
        assert!(layout
            .clean(row(11, &["1", "2", "3"]), &rule, &mut callback)
            .unwrap()
            .is_none());
        let valid = layout
            .clean(row(12, &["1", "2", ""]), &rule, &mut callback)
            .unwrap()
            .unwrap();
        assert_eq!(valid.cells.len(), 2);
        rule.column_range = Some([2, 2]);
        let projected = detect(&headers, &rule).unwrap();
        let valid = projected
            .clean(row(13, &["x", "2", "y"]), &rule, &mut callback)
            .unwrap()
            .unwrap();
        assert_eq!(valid.cells[0].value, "2");
        assert_eq!(valid.cells[0].col, 0);
        assert_eq!(diag[0].code, "extra_fields");
        assert_eq!(diag[1].code, "extra_empty_fields");
    }

    #[test]
    fn explicit_multi_rows_empty_names_and_repeated_headers() {
        let mut rule = SourceRule::default();
        rule.header_rows = vec![4, 5];
        rule.skip_repeated_headers = true;
        let layout = detect(
            &[row(4, &["Sales", "Sales"]), row(5, &["Order", "Amount"])],
            &rule,
        )
        .unwrap();
        assert_eq!(layout.headers, ["Sales / Order", "Sales / Amount"]);
        let mut diag = vec![];
        let removed = layout
            .clean(
                row(15, &[" sales   / order ", "SALES / AMOUNT"]),
                &rule,
                &mut |i| {
                    diag.push(i);
                    Ok(())
                },
            )
            .unwrap();
        assert!(removed.is_none());
        assert_eq!(diag[0].code, "repeated_header");
        rule.header_rows = vec![1];
        assert!(detect(&[row(1, &["ID", " id "])], &rule).is_err());
        rule.names = Some(vec!["Customer ID".into(), "Order ID".into()]);
        assert!(detect(&[row(1, &["ID", " id "])], &rule).is_ok());
    }

    #[test]
    fn headerless_sparse_coordinates_and_empty_states() {
        let mut rule = SourceRule::default();
        rule.headerless = true;
        rule.column_range = Some([10, 12]);
        let layout = detect(&[], &rule).unwrap();
        assert_eq!(layout.headers, ["column_10", "column_11", "column_12"]);
        let mut sparse = row(1048576, &["data"]);
        sparse.sparse = true;
        sparse.cells[0].col = 11;
        let row = layout
            .clean(sparse, &rule, &mut |_| {
                panic!("unexpected issue on sparse row")
            })
            .unwrap()
            .unwrap();
        assert_eq!(row.number, 1048576);
        assert_eq!(row.width, 3);
        assert_eq!(row.cells[0].col, 2);
        assert_eq!(row.cells.len(), 1);
    }
}
