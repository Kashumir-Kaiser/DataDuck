//! Exercise the real executable, its published artifacts, and its failure boundaries.
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;
use zip::write::SimpleFileOptions;

fn command(work: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dataduck"));
    cmd.current_dir(work);
    cmd
}

fn cli(work: &Path, args: &[&str], exit: i32) -> Output {
    let out = command(work).args(args).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(exit),
        "args {args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn report(path: &Path) -> Value {
    let html = fs::read_to_string(path).unwrap();
    let (_, body) = html
        .split_once(r#"id="report-data">"#)
        .expect("report data");
    let (data, _) = body.split_once("</script>").unwrap();
    let mut manifest: Value = serde_json::from_str(data).unwrap();
    for table in manifest["tables"].as_array_mut().unwrap() {
        if let Some(id) = table["profile_id"].as_str() {
            let marker = format!(r#"id="{id}">"#);
            let section = html
                .split_once(&marker)
                .unwrap()
                .1
                .split_once("</script>")
                .unwrap()
                .0;
            table["profile"] = serde_json::from_str(section).unwrap();
        }
    }
    manifest
}

fn analytical_profiles(doc: &Value) -> Vec<&Value> {
    doc["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| &t["profile"])
        .collect()
}

fn column<'a>(profile: &'a Value, name: &str) -> &'a Value {
    profile["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .expect("column in published report")
}

const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const DOC: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const SHEET: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

fn write_workbook(path: &Path, damage_second_sheet: bool) {
    let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
    let mut part = |name: &str, xml: &str| {
        zip.start_file(
            name,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(xml.as_bytes()).unwrap();
    };
    part(
        "[Content_Types].xml",
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/s1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/s2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>"#,
    );
    part(
        "_rels/.rels",
        &format!(
            r#"<Relationships xmlns="{PKG}"><Relationship Id="book" Type="{DOC}/officeDocument" Target="xl/workbook.xml"/></Relationships>"#
        ),
    );
    part(
        "xl/workbook.xml",
        &format!(
            r#"<workbook xmlns="{SHEET}" xmlns:r="{DOC}"><sheets><sheet name="Orders" sheetId="1" r:id="first"/><sheet name="Aux" sheetId="2" r:id="second"/></sheets></workbook>"#
        ),
    );
    part(
        "xl/_rels/workbook.xml.rels",
        &format!(
            r#"<Relationships xmlns="{PKG}"><Relationship Id="first" Type="{DOC}/worksheet" Target="worksheets/s1.xml"/><Relationship Id="second" Type="{DOC}/worksheet" Target="worksheets/s2.xml"/><Relationship Id="strings" Type="{DOC}/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#
        ),
    );
    part(
        "xl/sharedStrings.xml",
        &format!(
            r#"<sst xmlns="{SHEET}" count="3" uniqueCount="3"><si><t>ID</t></si><si><t>Region</t></si><si><r><t>We</t></r><r><t>st</t></r></si></sst>"#
        ),
    );
    part(
        "xl/worksheets/s1.xml",
        &format!(
            r#"<worksheet xmlns="{SHEET}"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>0007</t></is></c><c r="B2" t="s"><v>2</v></c></row></sheetData></worksheet>"#
        ),
    );
    let mut second = format!(
        r#"<worksheet xmlns="{SHEET}"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Quantity</t></is></c></row><row r="2"><c r="A2"><v>91</v></c></row>"#
    );
    if !damage_second_sheet {
        second.push_str("</sheetData></worksheet>");
    }
    part("xl/worksheets/s2.xml", &second);
    zip.finish().unwrap();
}

#[test]
fn local_workbook_and_reordered_csv_group_and_publish_exact_profiles_and_cleaned_records() {
    let temp = tempdir().unwrap();
    let w = temp.path();
    write_workbook(&w.join("a.xlsx"), false);
    fs::write(w.join("b.csv"), "Region,ID\nEast,0008\nWest,0009\n,0007\n").unwrap();
    cli(
        w,
        &[
            "build",
            "b.csv",
            "a.xlsx",
            "-o",
            "one.html",
            "--clean-dir",
            "clean",
        ],
        0,
    );
    cli(w, &["build", "a.xlsx", "b.csv", "-o", "two.html"], 0);
    let (first, second) = (report(&w.join("one.html")), report(&w.join("two.html")));
    assert_eq!(
        analytical_profiles(&first),
        analytical_profiles(&second),
        "argument order changed the profile or sample"
    );
    let tables = first["tables"].as_array().unwrap();
    assert_eq!(tables.len(), 2);
    let table = tables.iter().find(|t| t["rows"] == 4).unwrap();
    assert!(w
        .join("clean")
        .join(table["clean_file"].as_str().unwrap())
        .is_file());
    let id = column(&table["profile"], "ID");
    assert_eq!(id["kind"], "text");
    assert_eq!(id["distinct"], 3);
    assert_eq!(id["top"][0], json!({"value":"0007","count":2}));
    let region = column(&table["profile"], "Region");
    assert_eq!(region["valid"], 3);
    assert_eq!(region["missing"], 1);
    assert_eq!(region["missing_reasons"]["empty_cell"], 1);
    assert_eq!(region["distinct"], 2);
    assert_eq!(region["other"], 0);
    assert_eq!(
        region["top"],
        json!([{"value":"West","count":2},{"value":"East","count":1}])
    );
    let quantity_table = tables.iter().find(|t| t["rows"] == 1).unwrap();
    let quantity = column(&quantity_table["profile"], "Quantity");
    assert_eq!(quantity["bins"][0]["count"], 1);
    let mut exported = Vec::new();
    for entry in fs::read_dir(w.join("clean")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|v| v.to_str()) == Some("csv") {
            let mut csv = csv::Reader::from_path(path).unwrap();
            if csv.headers().unwrap().get(0) == Some("ID") {
                for record in csv.records() {
                    exported.push(
                        record
                            .unwrap()
                            .iter()
                            .map(str::to_owned)
                            .collect::<Vec<_>>(),
                    );
                }
            }
        }
    }
    assert_eq!(
        json!(exported),
        json!([
            ["0007", "West"],
            ["0008", "East"],
            ["0009", "West"],
            ["0007", ""]
        ])
    );
    // Re-importing this directory must not ingest DataDuck's own exported CSVs.
    cli(w, &["build", ".", "--recursive", "-o", "three.html"], 0);
    assert_eq!(
        analytical_profiles(&report(&w.join("three.html"))),
        analytical_profiles(&first)
    );
}

#[test]
fn partial_dashboard_drops_corrupt_sources_and_quarantines_known_rows() {
    let t = tempdir().unwrap();
    let w = t.path();
    write_workbook(&w.join("damaged.xlsx"), true);
    fs::write(w.join("broken.csv"), "ID,Region\nBAD,Poison\n1,\"truncated").unwrap();
    fs::write(
        w.join("valid.csv"),
        b"ID,Region\nabc,West\n\xff,x\nxyz,East\n",
    )
    .unwrap();
    cli(
        w,
        &[
            "build",
            ".",
            "-o",
            "report.html",
            "--issues",
            "audit.ndjson",
        ],
        2,
    );
    let result = report(&w.join("report.html"));
    let tables = result["tables"].as_array().unwrap();
    assert_eq!(tables.len(), 1, "{tables:?}");
    assert_eq!(tables[0]["rows"], 3);
    assert_eq!(column(&tables[0]["profile"], "Region")["distinct"], 2);
    let sources = result["sources"].as_array().unwrap();
    assert!(sources
        .iter()
        .any(|s| s["sheet"] == "Aux" && s["rows"] == 0));
    assert!(sources
        .iter()
        .any(|s| s["path"].as_str().unwrap().ends_with("broken.csv") && s["rows"] == 0));
    let audit = fs::read_to_string(w.join("audit.ndjson")).unwrap();
    let entries: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(entries
        .iter()
        .any(|entry| entry.to_string().contains("invalid_encoding")));
}

#[test]
fn fatal_build_preserves_previous_output_and_optional_exports() {
    let t = tempdir().unwrap();
    let w = t.path();
    fs::write(w.join("old.html"), b"old dashboard").unwrap();
    fs::write(w.join("broken.csv"), b"Id,Label\n1,\"open").unwrap();
    cli(
        w,
        &[
            "build",
            "broken.csv",
            "-o",
            "old.html",
            "--overwrite",
            "--clean-dir",
            "clean",
            "--issues",
            "issues.ndjson",
            "--temp-dir",
            "scratch",
        ],
        1,
    );
    assert_eq!(fs::read(w.join("old.html")).unwrap(), b"old dashboard");
    assert!(!w.join("clean").exists());
    assert!(!w.join("issues.ndjson").exists());
    assert_eq!(fs::read_dir(w.join("scratch")).unwrap().count(), 0);
}

#[test]
fn inspect_can_preview_an_explicit_sheet() {
    let t = tempdir().unwrap();
    write_workbook(&t.path().join("sheet.xlsx"), false);
    let result = cli(
        t.path(),
        &["inspect", "sheet.xlsx", "--sheet", "Orders", "--rows", "2"],
        0,
    );
    let preview = String::from_utf8(result.stdout).unwrap();
    assert!(
        preview.contains("Orders") && preview.contains("0007") && preview.contains("West"),
        "{preview}"
    );
}

#[cfg(unix)]
#[test]
fn interrupt_during_build_removes_scratch_and_preserves_previous_report() {
    use std::process::Stdio;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    let t = tempdir().unwrap();
    let w = t.path();
    // A sparse, very large field makes the child stay in ingestion until signalled.
    let mut input = File::create(w.join("large.csv")).unwrap();
    input.write_all(b"Id,Value\n1,").unwrap();
    input.set_len(1_100_000_000).unwrap();
    drop(input);
    fs::write(w.join("old.html"), b"previous report").unwrap();
    fs::create_dir(w.join("scratch")).unwrap();
    let mut child = command(w)
        .args([
            "build",
            "large.csv",
            "--temp-dir",
            "scratch",
            "--output",
            "old.html",
            "--overwrite",
            "--issues",
            "issues.ndjson",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while fs::read_dir(w.join("scratch")).unwrap().next().is_none() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before interruption: {status}");
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child never initialized scratch");
        }
        sleep(Duration::from_millis(3));
    }
    assert!(Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not finish cancellation");
        }
        sleep(Duration::from_millis(10));
    }
    assert_eq!(fs::read(w.join("old.html")).unwrap(), b"previous report");
    assert!(!w.join("issues.ndjson").exists());
    assert_eq!(fs::read_dir(w.join("scratch")).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn hundreds_of_inputs_work_with_a_low_descriptor_limit() {
    let t = tempdir().unwrap();
    for n in 0..160 {
        fs::write(t.path().join(format!("{n:04}.csv")), b"ID,Flag\n01,yes\n").unwrap();
    }
    let out = Command::new("sh")
        .current_dir(t.path())
        .arg("-c")
        .arg("ulimit -n 64 && exec \"$@\"")
        .arg("limit-fds")
        .arg(env!("CARGO_BIN_EXE_dataduck"))
        .args(["build", ".", "--quiet", "-o", "result.html"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = report(&t.path().join("result.html"));
    assert_eq!(v["sources"].as_array().unwrap().len(), 160);
    assert_eq!(v["tables"][0]["rows"], 160);
}
