//! Exercise the public exporter and data-only refresh boundary through the actual CLI.
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;
use walkdir::WalkDir;

fn cli(work: &Path, args: &[&str], code: i32) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_dataduck"))
        .current_dir(work)
        .arg("powerbi")
        .args(args)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(code), "{args:?}\n{stderr}");
    stderr
}

fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn snapshots(project: &Path) -> BTreeSet<PathBuf> {
    fs::read_dir(project.join("DataDuck/snapshots"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

fn inventory(project: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    WalkDir::new(project)
        .into_iter()
        .map(Result::unwrap)
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            (
                e.path().strip_prefix(project).unwrap().to_owned(),
                fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

fn records(path: &Path) -> Vec<BTreeMap<String, String>> {
    let mut reader = csv::Reader::from_path(path).unwrap();
    reader.deserialize().collect::<Result<_, _>>().unwrap()
}

fn decisions(snapshot: &Path) -> Vec<Value> {
    fs::read_to_string(snapshot.join("chart-decisions.ndjson"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn write_cars(work: &Path) {
    let mut w = csv::Writer::from_path(work.join("cars.csv")).unwrap();
    w.write_record(["Make", "Price", "Engine_Size", "VIN"])
        .unwrap();
    for i in 0..32 {
        let size = if i == 0 {
            String::new()
        } else {
            format!("{}.{:02}", 1 + i / 10, i % 10)
        };
        w.serialize((["Red", "Yellow"][i % 2], 100 + i, size, format!("{i:05}")))
            .unwrap();
    }
    w.flush().unwrap();
}

#[test]
fn export_publishes_filterable_facts_and_consistent_html_with_a_partial_source() {
    let t = tempdir().unwrap();
    let w = t.path();
    write_cars(w);
    fs::write(
        w.join("damaged.csv"),
        "Make,Price\nGhost,70000\n\"unterminated",
    )
    .unwrap();
    fs::write(
        w.join("charts.json"),
        json!({
            "version":1,"tables":[{"match":{"source":"cars.csv"},"fields":{
                "Price":{"role":"measure","aggregate":"average","priority":100},
                "Make":{"role":"category","priority":100},
                "Engine_Size":{"role":"numeric","priority":90}
            }}]
        })
        .to_string(),
    )
    .unwrap();
    cli(
        w,
        &[
            "export",
            "cars.csv",
            "damaged.csv",
            "--charts",
            "charts.json",
            "-o",
            "Car Report",
            "--html",
            "cars.html",
        ],
        2,
    );
    let project = w.join("Car Report");
    assert!(project.join("Car Report.pbip").is_file());
    assert!(project.join("Car Report.Report/definition.pbir").is_file());
    assert!(project
        .join("Car Report.SemanticModel/definition.pbism")
        .is_file());
    let contract = json_file(&project.join("DataDuck/contract.json"));
    let tables = contract["tables"].as_array().unwrap();
    assert_eq!(tables.len(), 1);
    let table = &tables[0];
    let id = table["id"].as_str().unwrap();
    assert!(table["charts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["axis"] == 0 && c["measure"] == 1 && c["aggregate"] == "average"));
    let snapshot = snapshots(&project).pop_first().unwrap();
    let fact = records(&snapshot.join(format!("fact-{id}.csv")));
    assert_eq!(fact.len(), 32);
    assert_eq!(fact[0]["F0004"], "00000");
    assert_eq!(fact[0]["Source"], "cars.csv");
    assert_eq!(fact[0]["Row"], "2");

    let makes = records(&snapshot.join(format!("axis-{id}-0001.csv")));
    let red_key = &makes.iter().find(|r| r["Label"] == "Red").unwrap()["Key"];
    let red: Vec<_> = fact.iter().filter(|r| &r["A0001"] == red_key).collect();
    assert_eq!(red.len(), 16);
    assert_eq!(
        red.iter()
            .map(|r| r["F0002"].parse::<u32>().unwrap())
            .sum::<u32>(),
        1840
    );
    let profile = records(&snapshot.join("profile.csv"));
    let size = profile
        .iter()
        .find(|r| r["Field"] == "Engine_Size")
        .unwrap();
    assert_eq!(
        (&size["Rows"][..], &size["Valid"][..], &size["Missing"][..]),
        ("32", "31", "1")
    );
    assert!(records(&snapshot.join("sources.csv"))
        .iter()
        .any(|r| r["Source"] == "damaged.csv" && r["Status"] == "skipped"));
    assert!(decisions(&snapshot)
        .iter()
        .any(|d| d["field"] == "Engine_Size" && d["rule"] == "histogram"));
    let html = fs::read_to_string(w.join("cars.html")).unwrap();
    let data = html
        .split_once(r#"id="report-data">"#)
        .unwrap()
        .1
        .split_once("</script>")
        .unwrap()
        .0;
    let manifest: Value = serde_json::from_str(data).unwrap();
    assert_eq!(manifest["tables"][0]["rows"], 32);
    assert_eq!(manifest["sources"].as_array().unwrap().len(), 2);
}

#[test]
fn updates_preserve_user_files_and_frozen_axes_and_accept_reordered_headers() {
    let t = tempdir().unwrap();
    let w = t.path();
    write_cars(w);
    cli(w, &["export", "cars.csv", "-o", "Report"], 0);
    let project = w.join("Report");
    let original = snapshots(&project);
    let first = original.first().unwrap();
    let contract = json_file(&project.join("DataDuck/contract.json"));
    let id = contract["tables"][0]["id"].as_str().unwrap();
    let axis_file = |field| format!("axis-{id}-{field:04}.csv");
    let initial_makes = records(&first.join(axis_file(1)));
    let initial_bins = records(&first.join(axis_file(3)));
    let title_path = WalkDir::new(project.join("Report.Report"))
        .into_iter()
        .map(Result::unwrap)
        .find(|e| e.file_name() == "page.json")
        .unwrap()
        .into_path();
    let mut page = json_file(&title_path);
    page["displayName"] = json!("My analysis");
    fs::write(title_path, serde_json::to_vec_pretty(&page).unwrap()).unwrap();
    let model_path = WalkDir::new(project.join("Report.SemanticModel/definition/tables"))
        .into_iter()
        .map(Result::unwrap)
        .find(|e| e.file_type().is_file())
        .unwrap()
        .into_path();
    let mut model = fs::read_to_string(&model_path).unwrap();
    model.push_str("\n\tmeasure 'My row count' = 42\n");
    fs::write(model_path, model).unwrap();
    let before = inventory(&project);

    cli(w, &["update-data", "cars.csv", "-o", "Report"], 0);
    assert_eq!(
        snapshots(&project),
        original,
        "an identical update must reuse the initialized snapshot"
    );
    assert_eq!(inventory(&project), before);

    fs::write(w.join("cars.csv"),"VIN,Engine_Size,Make,Price\n00002,0.10,Red,111\n00003,9.95,New make,222\n00004,,Yellow,333\n").unwrap();
    cli(w, &["update-data", "cars.csv", "-o", "Report"], 0);
    let after = snapshots(&project);
    assert_eq!(after.len(), 2);
    let second = after.difference(&original).next().unwrap();
    let second_rel = second.strip_prefix(&project).unwrap();
    let rest: BTreeMap<_, _> = inventory(&project)
        .into_iter()
        .filter(|(p, _)| !p.starts_with(second_rel))
        .collect();
    assert_eq!(
        rest, before,
        "refresh touched files outside its new snapshot"
    );
    let fact = records(&second.join(format!("fact-{id}.csv")));
    assert_eq!(fact.len(), 3);
    assert_eq!(
        (fact[1]["F0001"].as_str(), fact[1]["F0004"].as_str()),
        ("New make", "00003")
    );
    let makes = records(&second.join(axis_file(1)));
    for make in &initial_makes {
        assert!(makes.contains(make));
    }
    let other = makes.iter().find(|m| m["Label"] == "Other").unwrap();
    assert_eq!(fact[1]["A0001"], other["Key"]);
    let bins = records(&second.join(axis_file(3)));
    for bin in &initial_bins {
        assert!(bins.contains(bin));
    }
    for (row, label) in [(0, "Below range"), (1, "Above range"), (2, "Missing")] {
        let key = &bins.iter().find(|b| b["Label"] == label).unwrap()["Key"];
        assert_eq!(&fact[row]["A0003"], key);
    }

    cli(w, &["update-data", "cars.csv", "-o", "Report"], 0);
    assert_eq!(snapshots(&project), after);
    let stable = inventory(&project);
    for (invalid, needle) in [
        ("Make,Price,Engine_Size,VIN\nRed,oops,2.1,00001\n", "Price"),
        (
            "Make,Order_Date,Engine_Size,VIN\nRed,2025-01-01,2.1,00001\n",
            "price",
        ),
    ] {
        fs::write(w.join("cars.csv"), invalid).unwrap();
        let msg = cli(w, &["update-data", "cars.csv", "-o", "Report"], 1);
        assert!(msg.contains(needle), "{msg}");
        assert_eq!(inventory(&project), stable);
    }
}

#[test]
fn invalid_chart_settings_and_late_export_failure_publish_nothing() {
    let t = tempdir().unwrap();
    let w = t.path();
    write_cars(w);
    for fields in [
        json!({"Bogus":{"role":"category"}}),
        json!({"Make":{"role":"measure","aggregate":"sum"}}),
    ] {
        fs::write(
            w.join("bad.json"),
            json!({"version":1,"tables":[{"match":{"source":"cars.csv"},"fields":fields}]})
                .to_string(),
        )
        .unwrap();
        cli(
            w,
            &[
                "export",
                "cars.csv",
                "--charts",
                "bad.json",
                "-o",
                "Rejected",
                "--html",
                "rejected.html",
            ],
            64,
        );
        assert!(!w.join("Rejected").exists());
        assert!(!w.join("rejected.html").exists());
    }
    fs::write(w.join("old.html"), "my existing report").unwrap();
    cli(
        w,
        &["export", "cars.csv", "-o", "Rejected", "--html", "old.html"],
        1,
    );
    assert_eq!(fs::read(w.join("old.html")).unwrap(), b"my existing report");
    fs::write(
        w.join("wide.csv"),
        format!("Name,ID\n{},01\n", "x".repeat(32_001)),
    )
    .unwrap();
    let msg = cli(
        w,
        &[
            "export",
            "wide.csv",
            "-o",
            "Rejected",
            "--html",
            "rejected.html",
        ],
        1,
    );
    assert!(msg.contains("32,000"), "{msg}");
    assert!(!w.join("Rejected").exists());
    assert!(!w.join("rejected.html").exists());
}

#[test]
fn recursive_update_excludes_its_own_snapshot_csvs() {
    let t = tempdir().unwrap();
    let w = t.path();
    write_cars(w);
    cli(w, &["export", ".", "--recursive", "-o", "Inside"], 0);
    let before = snapshots(&w.join("Inside"));
    cli(w, &["update-data", ".", "--recursive", "-o", "Inside"], 0);
    assert_eq!(snapshots(&w.join("Inside")), before);
}

#[test]
fn calendar_refresh_retains_period_keys_and_rejects_an_incompatible_grain() {
    let t = tempdir().unwrap();
    let w = t.path();
    let input = w.join("orders.csv");
    fs::write(
        &input,
        "Order_Date,Label\n2025-02-02,\n2025-02-04,\n2025-02-05,\n",
    )
    .unwrap();
    cli(w, &["export", "orders.csv", "-o", "Orders"], 0);
    let project = w.join("Orders");
    let old = snapshots(&project);
    let table = json_file(&project.join("DataDuck/contract.json"));
    let id = table["tables"][0]["id"].as_str().unwrap();
    let axis = format!("axis-{id}-0001.csv");
    let original = records(&old.first().unwrap().join(&axis));
    assert!(
        original.iter().any(|row| row["Label"] == "2025-02-03"),
        "zero-record day is absent"
    );
    fs::write(
        &input,
        "Order_Date,Label\n2025-02-01,newly populated text\n2025-02-07,also text\n",
    )
    .unwrap();
    cli(w, &["update-data", "orders.csv", "-o", "Orders"], 0);
    let after = snapshots(&project);
    let latest = after.difference(&old).next().unwrap();
    let extended = records(&latest.join(axis));
    assert_eq!(extended.len(), 7);
    for item in original {
        assert!(extended.contains(&item));
    }
    let fact = records(&latest.join(format!("fact-{id}.csv")));
    assert_eq!(fact[0]["F0002"], "newly populated text");
    fs::write(
        &input,
        "Order_Date,Label\n2025-02-01,old\n2030-02-07,future\n",
    )
    .unwrap();
    let msg = cli(w, &["update-data", "orders.csv", "-o", "Orders"], 1);
    assert!(msg.contains("512"), "{msg}");
    assert_eq!(snapshots(&project), after);
}

#[test]
fn page_budget_keeps_unselected_tables_in_model_with_their_fact_rows() {
    let t = tempdir().unwrap();
    let w = t.path();
    fs::create_dir(w.join("inputs")).unwrap();
    for i in 1..=5u32 {
        let mut csv = format!("Make,Units{i}\n");
        for j in 0..(6 + i) {
            csv.push_str(&format!("{},{}\n", ["A", "B"][(j % 2) as usize], j));
        }
        fs::write(w.join("inputs").join(format!("t{i}.csv")), csv).unwrap();
    }
    cli(w, &["export", "inputs", "-o", "Many"], 0);
    let project = w.join("Many");
    let contract = json_file(&project.join("DataDuck/contract.json"));
    let table_array = contract["tables"].as_array().unwrap();
    assert_eq!(table_array.len(), 5);
    let no_charts: Vec<_> = table_array
        .iter()
        .filter(|t| t["charts"].as_array().unwrap().is_empty())
        .collect();
    assert_eq!(no_charts.len(), 1);
    let smallest = no_charts[0];
    assert_eq!(smallest["fields"][1]["name"], "Units1");
    let id = smallest["id"].as_str().unwrap();
    let snapshot = snapshots(&project).pop_first().unwrap();
    let fact = records(&snapshot.join(format!("fact-{id}.csv")));
    assert_eq!(fact.len(), 7);
    assert_eq!(fact[6]["F0002"], "6");
    assert!(decisions(&snapshot)
        .iter()
        .any(|record| record["table"] == id && record["status"] == "table_budget"));
    for table in table_array {
        let name = table["id"].as_str().unwrap();
        assert!(snapshot.join(format!("fact-{name}.csv")).is_file());
    }
}
