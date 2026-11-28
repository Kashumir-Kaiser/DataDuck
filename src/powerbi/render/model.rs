use super::{write_bytes, Names, MISSING, PROFILE, SOURCES};
use crate::powerbi::{facts, Aggregate, Contract, Table};
use crate::profile::ImportType;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

fn ident(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn dax_column(table: &str, field: &str) -> String {
    format!("{}[{}]", ident(table), field.replace(']', "]]"))
}

fn m_string(s: &str) -> String {
    let mut result = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => result.push_str("\"\""),
            '#' => result.push_str("#(#)"),
            c if c.is_control() => write!(result, "#({:04X})", c as u32).unwrap(),
            c => result.push(c),
        }
    }
    result.push('"');
    result
}

struct Column {
    name: String,
    source: String,
    kind: ImportType,
    hidden: bool,
    description: Option<String>,
}
impl Column {
    fn new(name: &str, kind: ImportType) -> Self {
        Self {
            name: name.into(),
            source: name.into(),
            kind,
            hidden: false,
            description: None,
        }
    }
}

fn model_type(kind: ImportType) -> (&'static str, &'static str, &'static str) {
    use ImportType::*;
    match kind {
        Text => ("string", "type text", ""),
        Boolean => ("boolean", "type logical", "Logical.FromText(_)"),
        Whole => ("int64", "Int64.Type", "if Text.Length(_) <= 15 then Int64.From(_, \"en-US\") else Int64.From(DataDuckExactDecimal(_))"),
        Fixed => ("decimal", "Currency.Type", "Currency.From(DataDuckExactDecimal(_))"),
        Double => ("double", "type number", "Number.FromText(_, \"en-US\")"),
        Date => (
            "dateTime",
            "type date",
            "Date.FromText(_, [Format=\"yyyy-MM-dd\", Culture=\"en-US\"])",
        ),
        DateTime => (
            "dateTime",
            "type datetime",
            "DateTime.FromText(Text.TrimEnd(_, {\"Z\"}), [Culture=\"en-US\"])",
        ),
    }
}

const AXIS_COLS: &[(&str, ImportType)] = &[
    ("Key", ImportType::Whole),
    ("Label", ImportType::Text),
    ("Sort", ImportType::Whole),
    ("Detail", ImportType::Text),
];
const PROFILE_COLS: &[(&str, ImportType)] = &[
    ("Table", ImportType::Text),
    ("Field", ImportType::Text),
    ("Kind", ImportType::Text),
    ("Rows", ImportType::Whole),
    ("Valid", ImportType::Whole),
    ("Missing", ImportType::Whole),
    ("Distinct", ImportType::Whole),
    ("Minimum", ImportType::Text),
    ("Maximum", ImportType::Text),
    ("Mean", ImportType::Double),
    ("Missing reasons", ImportType::Text),
];
const MISSING_COLS: &[(&str, ImportType)] = &[
    ("Table", ImportType::Text),
    ("Field", ImportType::Text),
    ("Reason", ImportType::Text),
    ("Count", ImportType::Whole),
];
const SOURCE_COLS: &[(&str, ImportType)] = &[
    ("Source", ImportType::Text),
    ("Sheet", ImportType::Text),
    ("Table", ImportType::Text),
    ("Status", ImportType::Text),
    ("Rows", ImportType::Whole),
    ("Rejected", ImportType::Whole),
    ("Message", ImportType::Text),
];

fn fixed_columns(cols: &[(&str, ImportType)]) -> Vec<Column> {
    cols.iter()
        .map(|(name, kind)| Column::new(name, *kind))
        .collect()
}

fn measure(out: &mut String, name: &str, expr: &str, format: Option<&str>) {
    writeln!(out, "\n\tmeasure {} = {expr}", ident(name)).unwrap();
    if let Some(f) = format {
        writeln!(out, "\t\tformatString: {f}").unwrap();
    }
}

fn partition(out: &mut String, table: &str, file: &str, cols: &[Column]) {
    writeln!(
        out,
        "\n\tpartition {} = m\n\t\tmode: import\n\t\tsource =\n\t\t\t\tlet",
        ident(table)
    )
    .unwrap();
    writeln!(out,"\t\t\t\t\tSource = Csv.Document(File.Contents(DataDuckSnapshotFolder & \"/\" & {}), [Delimiter=\",\", Columns={}, Encoding=65001, QuoteStyle=QuoteStyle.Csv, ExtraValues=ExtraValues.Error]),",m_string(file), cols.len()).unwrap();
    out.push_str("\t\t\t\t\tHeaders = Table.PromoteHeaders(Source, [PromoteAllScalars=true, Culture=\"en-US\"]),\n");
    // The partition schema is fixed. Reference exact source names rather than allowing
    // positional shifts or locale-based guessing. Empty CSV fields are null in the model.
    out.push_str("\t\t\t\t\tTyped = Table.TransformColumns(Headers, {\n");
    for (i, col) in cols.iter().enumerate() {
        let (_, m_type, parse) = model_type(col.kind);
        let expr = if parse.is_empty() { "_" } else { parse };
        let comma = if i + 1 == cols.len() { "" } else { "," };
        writeln!(out,"\t\t\t\t\t\t{{{}, each if _ = null or _ = \"\" then null else {expr}, {m_type}}}{comma}",m_string(&col.source)).unwrap();
    }
    out.push_str("\t\t\t\t\t})\n\t\t\t\tin\n\t\t\t\t\tTyped\n");
}

struct TableDef<'a> {
    name: &'a str,
    id: &'a str,
    file: &'a str,
    columns: Vec<Column>,
    measures: String,
    axis: bool,
}
fn table_file(dir: &Path, refs: &mut String, t: TableDef<'_>) -> Result<()> {
    let mut out = format!("table {}\n", ident(t.name));
    writeln!(refs, "ref table {}", ident(t.name))?;
    out.push_str(&t.measures);
    for col in &t.columns {
        out.push('\n');
        if let Some(description) = &col.description {
            // TMDL doc comments are line-oriented; every input byte stays on a comment line.
            let description: String = description
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            writeln!(out, "\t/// {description}")?;
        }
        writeln!(
            out,
            "\tcolumn {}\n\t\tdataType: {}\n\t\tsourceColumn: {}\n\t\tsummarizeBy: none",
            ident(&col.name),
            model_type(col.kind).0,
            col.source
        )?;
        match col.kind {
            ImportType::Date => out.push_str("\t\tformatString: yyyy-MM-dd\n\n\t\tannotation UnderlyingDateTimeDataType = Date\n"),
            ImportType::DateTime => out.push_str("\t\tformatString: yyyy-MM-dd HH:mm:ss.fff\n"),
            _=>{}
        }
        if col.hidden || (t.axis && matches!(col.name.as_str(), "Key" | "Sort")) {
            out.push_str("\t\tisHidden\n");
        }
        if t.axis && col.name == "Key" {
            out.push_str("\t\tisKey\n");
        }
        if t.axis && col.name == "Label" {
            out.push_str("\t\tsortByColumn: Sort\n");
        }
    }
    partition(&mut out, t.name, t.file, &t.columns);
    write_bytes(&dir.join(format!("tables/{}.tmdl", t.id)), out.as_bytes())
}

fn fact_measures(t: &Table, n: &Names) -> String {
    let mut out = String::new();
    measure(
        &mut out,
        &n.records(),
        &format!("COALESCE(COUNTROWS({}), 0)", ident(&n.fact)),
        Some("#,0"),
    );
    let mut used = HashSet::new();
    for chart in &t.charts {
        if let (Some(i), Some(agg)) = (chart.measure, chart.aggregate) {
            let name = n.aggregate(i, agg);
            let col = dax_column(&n.fact, &n.fields[i]);
            if used.insert(name.clone()) {
                let expr = match agg {
                    Aggregate::Average => "AVERAGE",
                    Aggregate::Sum => "SUM",
                    Aggregate::Min => "MIN",
                    Aggregate::Max => "MAX",
                };
                measure(
                    &mut out,
                    &name,
                    &format!("{expr}({col})"),
                    Some("#,0.###############"),
                );
            }
            let valid = n.valid(i);
            if used.insert(valid.clone()) {
                measure(
                    &mut out,
                    &valid,
                    &format!("COALESCE(COUNT({col}), 0)"),
                    Some("#,0"),
                );
            }
        }
    }
    out
}

pub(super) fn write(dir: &Path, snapshot: &Path, c: &Contract, entities: &[Names]) -> Result<()> {
    write_bytes(
        &dir.join("database.tmdl"),
        b"database\n\tcompatibilityLevel: 1601\n\tcompatibilityMode: powerBI\n",
    )?;
    let path = snapshot
        .to_str()
        .context("Power BI requires a Unicode snapshot path")?;
    // Standard Win32 paths also work when Power BI Desktop presents the parameter in its UI.
    let path = if let Some(p) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{p}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    };
    let mut parameter=format!("expression DataDuckSnapshotFolder = {} meta [IsParameterQuery=true, Type=\"Text\", IsParameterQueryRequired=true]\n\n\tannotation PBI_ResultType = Text\n",m_string(&path));
    // Parsing long CSV numbers directly with Number.FromText can round an integer or
    // fixed decimal before Int64.From/Currency.From sees it. Keep the model numeric.
    parameter.push_str("\nexpression DataDuckExactDecimal =\n");
    for line in include_str!("../../../assets/powerbi/exact-decimal.m").lines() {
        writeln!(parameter, "\t\t{line}")?;
    }
    parameter.push_str("\n\tannotation PBI_ResultType = Function\n");
    write_bytes(&dir.join("expressions.tmdl"), parameter.as_bytes())?;
    let mut refs=String::from("model Model\n\tculture: en-US\n\tdefaultPowerBIDataSourceVersion: powerBI_V3\n\tdiscourageImplicitMeasures\n\tsourceQueryCulture: en-US\n\nannotation __PBI_TimeIntelligenceEnabled = 0\n\n");
    let mut relationships = String::new();
    let mut accepted = String::new();
    measure(
        &mut accepted,
        "Accepted rows at export",
        &format!(
            "SUMX(VALUES({}), CALCULATE(MAX({})))",
            dax_column(PROFILE, "Table"),
            dax_column(PROFILE, "Rows")
        ),
        Some("#,0"),
    );
    for (name, id, schema, measures) in [
        (PROFILE, "profile", PROFILE_COLS, accepted),
        (MISSING, "missing", MISSING_COLS, String::new()),
        (SOURCES, "sources", SOURCE_COLS, String::new()),
    ] {
        table_file(
            dir,
            &mut refs,
            TableDef {
                name,
                id,
                file: &format!("{id}.csv"),
                columns: fixed_columns(schema),
                measures,
                axis: false,
            },
        )?;
    }
    for (t, n) in c.tables.iter().zip(entities) {
        crate::resource::check_cancel()?;
        let mut columns = Vec::with_capacity(t.fields.len() + 2 + t.axes.len());
        for (i, f) in t.fields.iter().enumerate() {
            let info = if f.utc {
                " (UTC)"
            } else if f.import == ImportType::Double {
                " (Power BI floating-point decimal)"
            } else {
                ""
            };
            columns.push(Column {
                name: n.fields[i].clone(),
                source: facts::field_header(i),
                kind: f.import,
                hidden: false,
                description: Some(format!("{}{info}", f.name)),
            });
        }
        columns.push(Column::new("Source", ImportType::Text));
        columns.push(Column {
            name: "Source row".into(),
            source: "Row".into(),
            kind: ImportType::Whole,
            hidden: false,
            description: None,
        });
        for axis in &t.axes {
            let i = axis.field;
            let key = format!("DD {}", facts::axis_header(i));
            let axis_name = n.axis(i);
            columns.push(Column {
                name: key.clone(),
                source: facts::axis_header(i),
                kind: ImportType::Whole,
                hidden: true,
                description: None,
            });
            let mut tooltip = String::new();
            measure(
                &mut tooltip,
                &n.detail(i),
                &format!("SELECTEDVALUE({})", dax_column(&axis_name, "Detail")),
                None,
            );
            table_file(
                dir,
                &mut refs,
                TableDef {
                    name: &axis_name,
                    id: &format!("{}-{}", t.id, i + 1),
                    file: &facts::axis_file(&t.id, i),
                    columns: fixed_columns(AXIS_COLS),
                    measures: tooltip,
                    axis: true,
                },
            )?;
            writeln!(relationships,"relationship dd_{}_{}\n\tfromColumn: {}.{}\n\ttoColumn: {}.Key\n\tcrossFilteringBehavior: oneDirection\n",t.id,i+1,ident(&n.fact),ident(&key),ident(&axis_name))?;
        }
        table_file(
            dir,
            &mut refs,
            TableDef {
                name: &n.fact,
                id: &t.id,
                file: &facts::fact_file(&t.id),
                columns,
                measures: fact_measures(t, n),
                axis: false,
            },
        )?;
    }
    if !relationships.is_empty() {
        write_bytes(&dir.join("relationships.tmdl"), relationships.as_bytes())?;
    }
    write_bytes(&dir.join("model.tmdl"), refs.as_bytes())
}
