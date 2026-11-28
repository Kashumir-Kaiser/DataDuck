//! Offline renderer for the native PBIR report and the TMDL import model.
mod model;
mod report;

use super::{Aggregate, Contract, Table};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

const PROFILE: &str = "Export profile";
const MISSING: &str = "Export missing reasons";
const SOURCES: &str = "Export sources";

fn json_schema(part: &str) -> String {
    format!("https://developer.microsoft.com/json-schemas/fabric/{part}/schema.json")
}

fn writer(path: &Path) -> Result<BufWriter<crate::VolumeWriter<File>>> {
    let parent = path.parent().context("generated file needs a directory")?;
    fs::create_dir_all(parent)?;
    let inner = File::create_new(path).with_context(|| format!("create {}", path.display()))?;
    Ok(BufWriter::new(crate::VolumeWriter {
        inner,
        dir: parent.into(),
        until: 0,
    }))
}
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut w = writer(path)?;
    w.write_all(bytes)?;
    w.flush()?;
    w.get_ref().inner.sync_all()?;
    Ok(())
}
fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut w = writer(path)?;
    serde_json::to_writer_pretty(&mut w, value)?;
    w.write_all(b"\n")?;
    w.flush()?;
    w.get_ref().inner.sync_all()?;
    Ok(())
}

struct Names {
    fact: String,
    fields: Vec<String>,
    number: usize,
}
impl Names {
    fn axis(&self, index: usize) -> String {
        format!("DD Axis {} {:04}", self.number, index + 1)
    }
    fn records(&self) -> String {
        format!("DD Records {}", self.number)
    }
    fn aggregate(&self, index: usize, aggregate: Aggregate) -> String {
        format!(
            "DD {} {} {:04}",
            aggregate_name(aggregate),
            self.number,
            index + 1
        )
    }
    fn valid(&self, index: usize) -> String {
        format!("DD Contributing rows {} {:04}", self.number, index + 1)
    }
    fn detail(&self, index: usize) -> String {
        format!("DD Bucket or value {} {:04}", self.number, index + 1)
    }
}
fn aggregate_name(a: Aggregate) -> &'static str {
    match a {
        Aggregate::Average => "Average",
        Aggregate::Sum => "Sum",
        Aggregate::Min => "Min",
        Aggregate::Max => "Max",
    }
}
// Use readable captions for fields and keep the physical partition headers independent.
// Reserve our namespace for globally unique measures, surrogate keys, and lookups.
fn alias(name: &str, used: &mut HashSet<String>) -> String {
    let base: String = name
        .chars()
        .take(72)
        .map(|c| {
            if c.is_control() || ".,;':/\\*|?[]{}\x60\x22<>=".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let base = base.trim();
    let base = if base.is_empty() {
        "Field".to_owned()
    } else if base.to_ascii_lowercase().starts_with("dd ") || base.eq_ignore_ascii_case("measures")
    {
        format!("Field {base}")
    } else {
        base.to_owned()
    };
    let mut candidate = base.clone();
    let mut n = 2;
    while !used.insert(candidate.to_lowercase()) {
        candidate = format!("{base} ({n})");
        n += 1;
    }
    candidate
}
fn names(table: &Table, number: usize, used: &mut HashSet<String>) -> Names {
    let fact = alias(&table.name, used);
    let mut columns = HashSet::from(["source".to_owned(), "source row".to_owned()]);
    let fields = table
        .fields
        .iter()
        .map(|f| alias(&f.name, &mut columns))
        .collect();
    Names {
        fact,
        fields,
        number,
    }
}

pub fn render(stage: &Path, name: &str, snapshot: &Path, c: &Contract) -> Result<()> {
    ensure!(c.template_version == 1, "unknown Power BI template version");
    let mut budget = 3 + 11 + 4 + 7; // three audit tables and their columns
    let mut used: HashSet<_> = [PROFILE, MISSING, SOURCES]
        .map(str::to_lowercase)
        .into_iter()
        .collect();
    let mut entities = Vec::new();
    for (i, t) in c.tables.iter().enumerate() {
        budget += 1 + t.fields.len() + 2 + t.axes.len() * 6; // lookup table + four attributes + fact foreign key
        ensure!(budget<=16_000,"Power BI model requires more than 16,000 columns plus tables; select fewer input columns");
        for ch in &t.charts {
            ensure!(
                t.axes.iter().any(|a| a.field == ch.axis) && ch.axis < t.fields.len(),
                "invalid chart axis"
            );
            ensure!(
                ch.measure.is_some() == ch.aggregate.is_some()
                    && ch.measure.is_none_or(|i| i < t.fields.len()),
                "invalid chart measure"
            );
        }
        entities.push(names(t, i + 1, &mut used));
    }
    let report_dir = format!("{name}.Report");
    let model_dir = format!("{name}.SemanticModel");
    write_json(
        &stage.join(format!("{name}.pbip")),
        &json!({
            "$schema":json_schema("pbip/pbipProperties/1.0.0"),"version":"1.0","artifacts":[{"report":{"path":report_dir}}]
        }),
    )?;
    let report = stage.join(&report_dir);
    write_json(
        &report.join("definition.pbir"),
        &json!({
            "$schema":json_schema("item/report/definitionProperties/2.0.0"),"version":"4.0","datasetReference":{"byPath":{"path":format!("../{model_dir}")}}
        }),
    )?;
    let model = stage.join(model_dir);
    write_json(
        &model.join("definition.pbism"),
        &json!({
            "$schema":json_schema("item/semanticModel/definitionProperties/1.0.0"),"version":"4.0","settings":{"qnaEnabled":false}
        }),
    )?;
    write_bytes(
        &stage.join("DataDuck/template-version.json"),
        include_bytes!("../../assets/powerbi/template-version.json"),
    )?;
    write_bytes(
        &stage.join("DataDuck/LICENSE-Microsoft.txt"),
        include_bytes!("../../assets/powerbi/LICENSE-Microsoft.txt"),
    )?;
    model::write(&model.join("definition"), snapshot, c, &entities)?;
    report::write(&report, c, &entities)
}
