//! Emit a portable report without holding its HTML or complete table profiles in RAM.
use crate::manifest::SourceList;
use anyhow::{ensure, Context, Result};
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub struct TableSection {
    pub name: String,
    pub columns: usize,
    pub rows: u64,
    pub profile: PathBuf,
    pub clean_file: Option<String>,
}

const HEAD: &str = include_str!("../assets/dashboard.html");
const BODY: &str = include_str!("../assets/dashboard-body.html");
const STYLE: &str = include_str!("../assets/dashboard.css");
const UPLOT_CSS: &str = include_str!("../assets/uPlot-1.6.32.min.css");
const UPLOT: &str = include_str!("../assets/uPlot-1.6.32.min.js");
const LICENSE: &str = include_str!("../assets/uPlot-LICENSE.txt");
const APP: &str = include_str!("../assets/dashboard.js");

/// JSON stays valid after these escapes, including inside an inert HTML script element.
struct HtmlJson<W>(W);
impl<W: Write> Write for HtmlJson<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut start = 0;
        for (offset, &byte) in bytes.iter().enumerate() {
            if byte == b'<' {
                self.0.write_all(&bytes[start..offset])?;
                self.0.write_all(br"\u003c")?;
                start = offset + 1;
            }
        }
        self.0.write_all(&bytes[start..])?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

struct Counter(u64);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("report size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ProfileSize {
    full: u64,
    // Raw file offset immediately after the opening bracket of the final sample array.
    sample_start: u64,
    compact: u64,
    omit_sample: bool,
}

/// The engine writes trusted JSON with a final top-level `sample` array. Scan its
/// structural bytes rather than materializing a column (one label can be 1 MiB).
fn measure_profile(path: &Path) -> Result<ProfileSize> {
    let mut file =
        File::open(path).with_context(|| format!("read table profile {}", path.display()))?;
    let mut buffer = [0; 32768];
    let (mut offset, mut escaped_len, mut depth) = (0_u64, 0_u64, 0_u32);
    let (mut quoted, mut escape) = (false, false);
    let mut key_len: Option<usize> = None;
    let mut sample_key = false;
    let mut start = None;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        for &b in &buffer[..n] {
            offset += 1;
            escaped_len += if b == b'<' { 6 } else { 1 };
            if quoted {
                if escape {
                    escape = false;
                    continue;
                }
                match b {
                    b'\\' => {
                        escape = true;
                        key_len = None;
                    }
                    b'"' => {
                        quoted = false;
                        sample_key = key_len == Some(6);
                    }
                    _ => {
                        key_len = key_len
                            .filter(|&i| b"sample".get(i) == Some(&b))
                            .map(|i| i + 1)
                    }
                }
            } else {
                match b {
                    b'"' => {
                        quoted = true;
                        key_len = (depth == 1).then_some(0);
                    }
                    b'[' | b'{' => {
                        if depth == 1 && b == b'[' && sample_key {
                            start = Some((offset, escaped_len + 2));
                        } else if depth == 1 && start.is_some() {
                            anyhow::bail!("unexpected field after profile sample");
                        }
                        sample_key = false;
                        depth += 1;
                    }
                    b']' | b'}' => {
                        depth = depth
                            .checked_sub(1)
                            .context("invalid table profile nesting")?
                    }
                    b',' if depth == 1 && start.is_some() => {
                        anyhow::bail!("profile sample must be the final field")
                    }
                    _ => {}
                }
            }
        }
    }
    ensure!(
        !quoted && depth == 0,
        "incomplete table profile {}",
        path.display()
    );
    let (sample_start, compact) = start.context("table profile is missing its sample array")?;
    Ok(ProfileSize {
        full: escaped_len,
        sample_start,
        compact,
        omit_sample: false,
    })
}

fn write_document<W: Write>(
    output: &mut W,
    sources: &(impl SourceList + ?Sized),
    tables: &[TableSection],
    sizes: &[ProfileSize],
    include_examples: bool,
    include_payload: bool,
) -> Result<()> {
    for piece in [HEAD, UPLOT_CSS, STYLE, BODY] {
        output.write_all(piece.as_bytes())?;
    }
    write!(
        output,
        "{{\"version\":2,\"generator_version\":\"{}\",\"examples_omitted\":{},\"sources\":",
        env!("CARGO_PKG_VERSION"),
        u8::from(!include_examples)
    )?;
    sources.serialize_to(HtmlJson(&mut *output), include_examples)?;
    output.write_all(br#","tables":["#)?;
    for (i, (table, size)) in tables.iter().zip(sizes).enumerate() {
        if i > 0 {
            output.write_all(b",")?;
        }
        output.write_all(br#"{"name":"#)?;
        serde_json::to_writer(HtmlJson(&mut *output), &table.name)?;
        output.write_all(br#","clean_file":"#)?;
        serde_json::to_writer(HtmlJson(&mut *output), &table.clean_file)?;
        write!(
            output,
            ",\"rows\":{},\"columns\":{},\"sample_omitted\":{},\"profile_id\":\"profile-{}\"}}",
            table.rows,
            table.columns,
            u8::from(size.omit_sample),
            i
        )?;
    }
    output.write_all(b"]}\n</script>\n")?;
    // The browser can switch between these sections without parsing the entire dataset.
    for (i, (table, size)) in tables.iter().zip(sizes).enumerate() {
        writeln!(
            output,
            "<script type=\"application/json\" id=\"profile-{i}\">"
        )?;
        if include_payload {
            let mut file = BufReader::new(File::open(&table.profile)?);
            if size.omit_sample {
                let copied = io::copy(
                    &mut file.take(size.sample_start),
                    &mut HtmlJson(&mut *output),
                )?;
                ensure!(
                    copied == size.sample_start,
                    "table profile changed during rendering"
                );
                output.write_all(b"]}")?;
            } else {
                io::copy(&mut file, &mut HtmlJson(&mut *output))?;
            }
        }
        output.write_all(b"\n</script>\n")?;
    }
    output.write_all(b"<script>\n/*\nuPlot 1.6.32\n")?;
    for piece in [
        LICENSE,
        "\n*/\n",
        UPLOT,
        "\n",
        APP,
        "\n</script>\n</body>\n</html>\n",
    ] {
        output.write_all(piece.as_bytes())?;
    }
    Ok(())
}

pub fn write_report(
    dest: &mut impl Write,
    sources: &(impl SourceList + ?Sized),
    tables: &[TableSection],
    limit: usize,
) -> Result<()> {
    let mut sizes: Vec<_> = tables
        .iter()
        .map(|table| measure_profile(&table.profile))
        .collect::<Result<_>>()?;
    let mut fixed = Counter(0);
    write_document(&mut fixed, sources, tables, &sizes, true, false)?;
    let mut bytes = sizes
        .iter()
        .try_fold(fixed.0, |sum, s| sum.checked_add(s.full))
        .context("report size overflow")?;
    for size in sizes.iter_mut().rev() {
        if bytes <= limit as u64 {
            break;
        }
        let saving = size.full.saturating_sub(size.compact);
        if saving > 0 {
            size.omit_sample = true;
            bytes -= saving;
        }
    }
    let include_examples = bytes <= limit as u64;
    if !include_examples {
        let mut smaller = Counter(0);
        write_document(&mut smaller, sources, tables, &sizes, false, false)?;
        bytes = bytes - fixed.0 + smaller.0;
    }
    ensure!(bytes <= limit as u64, "core dashboard needs {bytes} bytes, above the {limit}-byte HTML limit; increase --max-html-mib or select fewer inputs");
    write_document(dest, sources, tables, &sizes, include_examples, true)?;
    dest.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, Source};
    use serde_json::{json, Value};

    fn profile(dir: &Path, name: &str, sample: &str) -> TableSection {
        let profile = dir.join(name);
        let data = json!({
            "rows": 1,
            "columns": [{"name": "</ScRiPt>", "kind":"text", "valid":1, "missing":0,
                         "distinct":1, "top":[{"value":"[, \\\"sample\\\": [] <&", "count":1}],
                         "other":0, "min":null, "max":null, "mean":null, "bins":[],
                         "missing_reasons":{}}],
            "sample":[{"source":"src", "row":2, "values":[sample]}]
        });
        // Store fields in the same order as the engine, independently of map implementation.
        let mut file = File::create(&profile).unwrap();
        write!(
            file,
            "{{\"rows\":1,\"columns\":{},\"sample\":{}}}",
            data["columns"], data["sample"]
        )
        .unwrap();
        TableSection {
            name: String::from(name) + "</script>",
            columns: 1,
            rows: 1,
            profile,
            clean_file: None,
        }
    }

    fn payload(html: &[u8]) -> Value {
        let html = std::str::from_utf8(html).unwrap();
        fn section(html: &str, id: &str) -> Value {
            let marker = format!("<script type=\"application/json\" id=\"{id}\">");
            let raw = html
                .split_once(&marker)
                .unwrap()
                .1
                .split_once("</script>")
                .unwrap()
                .0;
            assert!(!raw.contains('<'));
            serde_json::from_str(raw).unwrap()
        }
        let mut data = section(html, "report-data");
        for table in data["tables"].as_array_mut().unwrap() {
            assert!(table.get("profile").is_none());
            table["profile"] = section(html, table["profile_id"].as_str().unwrap());
        }
        data
    }

    #[test]
    fn data_round_trips_safely_and_limits_count_escaped_html() {
        let dir = tempfile::tempdir().unwrap();
        let tables = [
            profile(dir.path(), "a", "literal </script> & β"),
            profile(dir.path(), "b", &"<".repeat(20_000)),
        ];
        let inputs = [Source {
            path: "<local>".into(),
            examples: vec![Issue {
                message: "x".repeat(2000),
                ..Default::default()
            }],
            ..Default::default()
        }];
        let mut full = vec![];
        write_report(&mut full, &inputs[..], &tables, usize::MAX).unwrap();
        let data = payload(&full);
        assert_eq!(data["version"], 2);
        assert_eq!(data["generator_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(data["sources"][0]["path"], "<local>");
        assert_eq!(
            data["tables"][0]["profile"]["sample"][0]["values"][0],
            "literal </script> & β"
        );

        let mut same = Vec::new();
        write_report(&mut same, &inputs[..], &tables, full.len()).unwrap();
        assert_eq!(same, full);
        let mut smaller = Vec::new();
        write_report(&mut smaller, &inputs[..], &tables, full.len() - 1).unwrap();
        assert!(smaller.len() < full.len());
        let small = payload(&smaller);
        assert_eq!(small["tables"][1]["sample_omitted"], 1);
        assert_eq!(small["tables"][1]["profile"]["sample"], json!([]));
        assert_eq!(small["tables"][0], data["tables"][0]);
        assert_eq!(
            small["tables"][1]["profile"]["columns"],
            data["tables"][1]["profile"]["columns"]
        );

        let mut no_samples = Vec::new();
        write_report(&mut no_samples, &inputs[..], &tables, smaller.len() - 1).unwrap();
        assert_eq!(payload(&no_samples)["examples_omitted"], 0);
        let mut no_examples = Vec::new();
        write_report(&mut no_examples, &inputs[..], &tables, no_samples.len() - 1).unwrap();
        let minimal = payload(&no_examples);
        assert_eq!(minimal["examples_omitted"], 1);
        assert_eq!(minimal["sources"][0]["examples"], json!([]));
        assert_eq!(minimal["sources"][0]["path"], "<local>");

        let mut dest = Vec::new();
        assert!(write_report(&mut dest, &inputs[..], &tables, 100).is_err());
        assert!(dest.is_empty());
    }

    #[test]
    fn refuses_truncated_or_reordered_private_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile");
        for invalid in [
            r#"{"columns":[],"sample":["#,
            r#"{"sample":[],"rows":2}"#,
            r#"{"columns":[]}"#,
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(measure_profile(&path).is_err(), "{invalid}");
        }
    }
}
