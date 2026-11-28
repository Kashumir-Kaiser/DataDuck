use super::{json_schema, write_bytes, write_json, Names, PROFILE, SOURCES};
use crate::powerbi::{facts, Chart, Contract, Visual};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

const TEMPLATE: &str = include_str!("../../../assets/powerbi/visual-roles.json");

fn schema(item: &str) -> String {
    json_schema(&format!("item/report/definition/{item}"))
}
fn expression(table: &str, field: &str, measure: bool) -> Value {
    let object = if measure { "Measure" } else { "Column" };
    json!({(object): {"Expression":{"SourceRef":{"Entity":table}},"Property":field}})
}

fn projection(table: &str, field: &str, measure: bool, caption: &str) -> Value {
    json!({"field":expression(table,field,measure),"queryRef":format!("{table}.{field}"),"nativeQueryRef":field,"displayName":caption})
}
struct Tile<'a> {
    kind: &'a str,
    title: &'a str,
    roles: Vec<(&'a str, Vec<Value>)>,
    sort: Option<Value>,
    dimension: Option<&'a str>,
}
struct Page<'a> {
    dir: &'a Path,
    templates: &'a Value,
    id: String,
    title: String,
    tiles: Vec<String>,
}
impl<'a> Page<'a> {
    fn new(dir: &'a Path, templates: &'a Value, id: &str, title: &str) -> Self {
        Self {
            dir,
            templates,
            id: id.into(),
            title: title.into(),
            tiles: Vec::new(),
        }
    }
    fn tile(&mut self, t: Tile<'_>, rect: [u32; 4]) -> Result<()> {
        let i = self.tiles.len();
        let id = format!("v{:02}", i + 1);
        let mut v = self.templates[t.kind].clone();
        let q = &mut v["query"];
        for (role, values) in t.roles {
            q["queryState"][role] = json!({"projections":values});
        }
        if let Some(field) = t.sort {
            q["sortDefinition"] =
                json!({"sort":[{"field":field,"direction":"Ascending"}],"isDefaultSort":false});
        }
        v["visualContainerObjects"] = json!({"title":[{"properties":{"show":{"expr":{"Literal":{"Value":"true"}}},"text":{"expr":{"Literal":{"Value":format!("'{}'",t.title.replace('\'',"''"))}}}}}]});
        let [x, y, width, height] = rect;
        let mut visual = json!({"$schema":schema("visualContainer/2.4.0"),"name":id,"position":{"x":x,"y":y,"width":width,"height":height,"z":i,"tabOrder":i},"visual":v});
        if let Some(table) = t.dimension {
            // Keys on valid calendar bins can be negative; compare with the reserved
            // missing sentinel, never with zero or the display label "Missing".
            visual["filterConfig"] = json!({"filters":[{
                "name":format!("filter_{}_{}",self.id,id),"field":expression(table,"Key",false),"type":"Advanced",
                "filter":{"Version":2,"From":[{"Name":"d","Entity":table,"Type":0}],
                    "Where":[{"Condition":{"Comparison":{"ComparisonKind":1,
                      "Left":{"Column":{"Expression":{"SourceRef":{"Source":"d"}},"Property":"Key"}},
                      "Right":{"Literal":{"Value":format!("{}L",facts::MISSING)}}
                    }}}]}
            }]});
        }
        write_json(
            &self
                .dir
                .join(&self.id)
                .join("visuals")
                .join(&id)
                .join("visual.json"),
            &visual,
        )?;
        self.tiles.push(id);
        Ok(())
    }
    fn finish(self, analysis: bool) -> Result<String> {
        let mut p = json!({"$schema":schema("page/2.0.0"),"name":self.id,"displayName":self.title,"displayOption":"FitToPage","width":1280,"height":720});
        if analysis {
            let mut interactions = Vec::new();
            for source in &self.tiles {
                for target in &self.tiles {
                    if source != target {
                        interactions
                            .push(json!({"source":source,"target":target,"type":"DataFilter"}));
                    }
                }
            }
            p["visualInteractions"] = Value::Array(interactions);
        }
        write_json(&self.dir.join(&self.id).join("page.json"), &p)?;
        Ok(self.id)
    }
}

fn analysis_tile(page: &mut Page<'_>, n: &Names, chart: &Chart, index: usize) -> Result<()> {
    let dim = n.axis(chart.axis);
    let category = &n.fields[chart.axis];
    let mut axis = projection(&dim, "Label", false, category);
    axis["active"] = Value::Bool(true);
    let count = n.records();
    let detail = n.detail(chart.axis);
    let mut tips = vec![projection(&dim, &detail, true, "Bucket or value")];
    let (measure, caption) = match (chart.measure, chart.aggregate) {
        (Some(m), Some(a)) => {
            tips.push(projection(
                &n.fact,
                &n.valid(m),
                true,
                "Contributing records",
            ));
            (
                n.aggregate(m, a),
                format!("{} {}", super::aggregate_name(a), n.fields[m]),
            )
        }
        _ => (count, "Records".into()),
    };
    let kind = match chart.visual {
        Visual::Bars => "clusteredBarChart",
        Visual::Columns => "clusteredColumnChart",
        Visual::Line => "lineChart",
    };
    let x = 16 + (index % 2) as u32 * 632;
    let y = 16 + (index / 2) as u32 * 352;
    page.tile(
        Tile {
            kind,
            title: &chart.title,
            roles: vec![
                ("Category", vec![axis]),
                ("Y", vec![projection(&n.fact, &measure, true, &caption)]),
                ("Tooltips", tips),
            ],
            sort: Some(expression(&dim, "Label", false)),
            dimension: Some(&dim),
        },
        [x, y, 616, 336],
    )
}

fn profile_page(pages: &Path, templates: &Value) -> Result<String> {
    let mut page = Page::new(
        pages,
        templates,
        "ExportProfile",
        "Export profile · snapshot",
    );
    for (name, rect) in [
        ("Table", [16, 16, 380, 100]),
        ("Field", [408, 16, 470, 100]),
    ] {
        page.tile(
            Tile {
                kind: "slicer",
                title: name,
                roles: vec![("Values", vec![projection(PROFILE, name, false, name)])],
                sort: None,
                dimension: None,
            },
            rect,
        )?;
    }
    page.tile(
        Tile {
            kind: "cardVisual",
            title: "Accepted rows at export",
            roles: vec![(
                "Data",
                vec![projection(
                    PROFILE,
                    "Accepted rows at export",
                    true,
                    "Accepted rows",
                )],
            )],
            sort: None,
            dimension: None,
        },
        [890, 16, 374, 100],
    )?;
    let mut values = Vec::new();
    for col in [
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
    ] {
        values.push(projection(PROFILE, col, false, col));
    }
    page.tile(
        Tile {
            kind: "tableEx",
            title: "Column profile at export",
            roles: vec![("Values", values)],
            sort: None,
            dimension: None,
        },
        [16, 132, 1248, 322],
    )?;
    let audit = [
        "Source", "Sheet", "Table", "Status", "Rows", "Rejected", "Message",
    ]
    .iter()
    .map(|col| projection(SOURCES, col, false, col))
    .collect();
    page.tile(
        Tile {
            kind: "tableEx",
            title: "Source audit · all input sources",
            roles: vec![("Values", audit)],
            sort: None,
            dimension: None,
        },
        [16, 470, 1248, 234],
    )?;
    page.finish(false)
}

pub(super) fn write(dir: &Path, c: &Contract, entities: &[Names]) -> Result<()> {
    let def = dir.join("definition");
    let pages = def.join("pages");
    write_bytes(
        &dir.join("StaticResources/SharedResources/BaseThemes/CY19SU12.json"),
        include_bytes!("../../../assets/powerbi/CY19SU12.json"),
    )?;
    write_bytes(
        &def.join("report.json"),
        include_bytes!("../../../assets/powerbi/report.json"),
    )?;
    write_json(
        &def.join("version.json"),
        &json!({"$schema":schema("versionMetadata/1.0.0"),"version":"2.0.0"}),
    )?;
    let templates: Value = serde_json::from_str(TEMPLATE)?;
    let mut page_ids = Vec::new();
    for (t, n) in c.tables.iter().zip(entities) {
        for (i, charts) in t.charts.chunks(4).enumerate() {
            let title = if i == 0 {
                n.fact.clone()
            } else {
                format!("{} · distributions", n.fact)
            };
            let mut p = Page::new(
                &pages,
                &templates,
                &format!("Analysis{:04}P{}", n.number, i + 1),
                &title,
            );
            for (j, ch) in charts.iter().enumerate() {
                analysis_tile(&mut p, n, ch, j)?;
            }
            page_ids.push(p.finish(true)?);
        }
    }
    page_ids.push(profile_page(&pages, &templates)?);
    write_json(
        &pages.join("pages.json"),
        &json!({"$schema":schema("pagesMetadata/1.0.0"),"activePageName":page_ids[0],"pageOrder":page_ids}),
    )?;
    Ok(())
}
