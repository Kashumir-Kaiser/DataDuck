//! Power BI uses the same committed row segments and full-column inference as HTML.
use super::facts::{FactWriter, SnapshotWriter};
use super::planner::{self, ChartConfig, TableRules};
use super::storage::Stage;
use super::{AxisSpec, Contract, Field, Table};
use crate::layout::header_key;
use crate::profile::{ImportType, Profile, Profiler, Type};
use crate::{
    config, destination, discover, ingest, parent, report, resource, same_dest, scratch, Common,
    Group, InvalidSettings, VolumeWriter,
};
use anyhow::{ensure, Context, Result};
use clap::{Args, Subcommand};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Generate an editable local Power BI Desktop project in a fresh folder.
    Export {
        #[command(flatten)]
        input: Inputs,
        /// Versioned JSON chart roles and priorities.
        #[arg(long)]
        charts: Option<PathBuf>,
        /// Also publish a standalone HTML report at this fresh path.
        #[arg(long)]
        html: Option<PathBuf>,
    },
    /// Add an immutable data snapshot to an existing project.
    UpdateData {
        #[command(flatten)]
        input: Inputs,
    },
}

#[derive(Args)]
pub(crate) struct Inputs {
    #[arg(required = true)]
    inputs: Vec<PathBuf>,
    /// Power BI project directory.
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long)]
    recursive: bool,
    #[arg(long)]
    quiet: bool,
    #[command(flatten)]
    common: Common,
}

fn table_id(headers: &[String]) -> String {
    let mut keys: Vec<_> = headers.iter().map(|name| header_key(name)).collect();
    keys.sort_unstable();
    let mut digest = Sha256::new();
    for key in keys {
        digest.update((key.len() as u64).to_le_bytes());
        digest.update(key.as_bytes());
    }
    format!("t_{:x}", digest.finalize())
}

fn headers(group: &Group) -> Result<Vec<String>> {
    Ok(serde_json::from_reader(BufReader::new(
        group.headers.open()?,
    ))?)
}

struct InputTable {
    group: Group,
    id: String,
    rules: TableRules,
    saved: Option<Table>,
}

fn schema_mismatch(actual: &[String], tables: &BTreeMap<String, Table>) -> anyhow::Error {
    let new: BTreeSet<_> = actual.iter().map(|s| header_key(s)).collect();
    let closest = tables
        .values()
        .max_by_key(|t| t.fields.iter().filter(|f| new.contains(&f.key)).count());
    let (table, old) = match closest {
        Some(t) => (
            t.name.as_str(),
            t.fields.iter().map(|f| f.key.clone()).collect(),
        ),
        None => ("(new logical table)", BTreeSet::new()),
    };
    let added: Vec<_> = new.difference(&old).take(12).collect();
    let removed: Vec<_> = old.difference(&new).take(12).collect();
    anyhow::anyhow!("schema changed near {table}: added {added:?}, missing {removed:?}; run powerbi export into a fresh folder")
}

fn prepare(
    groups: Vec<Group>,
    cfg: &ChartConfig,
    baseline: Option<Contract>,
) -> Result<(Vec<InputTable>, BTreeSet<String>)> {
    let updating = baseline.is_some();
    let mut saved: BTreeMap<_, _> = baseline
        .into_iter()
        .flat_map(|c| c.tables)
        .map(|t| (t.id.clone(), t))
        .collect();
    let mut list = Vec::new();
    let mut used = BTreeSet::new();
    let mut names_bytes = 0usize;
    let mut columns_and_tables = 0usize;
    for group in groups {
        resource::check_cancel()?;
        let names = headers(&group)?;
        names_bytes += names.iter().map(|s| s.len()).sum::<usize>();
        ensure!(
            names_bytes <= 16 << 20,
            "Power BI model field names exceed 16 MiB; select fewer columns in the source recipe"
        );
        columns_and_tables += names.len() + 3; // table, source name, source row number
        ensure!(columns_and_tables <= 16_000, "Power BI model exceeds 16,000 tables plus columns; select fewer fields in the source recipe");
        let id = table_id(&names);
        let old = if updating {
            Some(
                saved
                    .remove(&id)
                    .ok_or_else(|| schema_mismatch(&names, &saved))?,
            )
        } else {
            None
        };
        let sources: Vec<_> = group
            .parts
            .iter()
            .map(|p| {
                (
                    p.source[..p.path_len].to_owned(),
                    p.source.get(p.path_len + 3..).map(str::to_owned),
                )
            })
            .collect();
        let rules = cfg
            .resolve(&sources, &names, &mut used)
            .context(InvalidSettings)?;
        list.push(InputTable {
            group,
            id,
            rules,
            saved: old,
        });
    }
    cfg.check_all_matched(&used).context(InvalidSettings)?;
    ensure!(
        saved.is_empty(),
        "schema changed: missing logical tables {:?}; run powerbi export into a fresh folder",
        saved.values().map(|t| &t.name).collect::<Vec<_>>()
    );
    let mut ranked = list.iter().collect::<Vec<_>>();
    ranked.sort_unstable_by(|a, b| {
        b.rules
            .priority
            .cmp(&a.rules.priority)
            .then_with(|| b.group.accepted.cmp(&a.group.accepted))
            .then_with(|| a.id.cmp(&b.id))
    });
    let chosen = ranked.into_iter().take(4).map(|g| g.id.clone()).collect();
    Ok((list, chosen))
}

fn update_column(table: &mut Table, i: usize, col: &crate::profile::Column) -> Result<()> {
    let field = &table.fields[i];
    ensure!(
        field.key == header_key(&col.name),
        "column mismatch for {}",
        field.name
    );
    let compatible_kind = col.valid == 0
        || match field.kind {
            Type::Integer | Type::Decimal => matches!(col.kind, Type::Integer | Type::Decimal),
            Type::Datetime | Type::Date => matches!(col.kind, Type::Datetime | Type::Date),
            Type::Empty | Type::Text => true,
            Type::Boolean => col.kind == Type::Boolean,
        };
    ensure!(compatible_kind && col.supports_import(field.import)?
        && !(col.valid > 0 && field.import == ImportType::DateTime && field.utc != col.utc()),
        "schema changed for {}.{}: saved {:?} ({:?}), incoming {:?} ({:?}); run powerbi export into a fresh folder",
        table.name, field.name, field.kind, field.import, col.kind, col.import_type()?);
    if col.valid > 0 {
        if let Some(axis) = table.axes.iter_mut().find(|a| a.field == i) {
            match &mut axis.spec {
                AxisSpec::Calendar(calendar) => calendar.extend(col.min.as_deref().context("missing calendar minimum")?, col.max.as_deref().context("missing calendar maximum")?)
                    .with_context(|| format!("{} calendar range changed; run powerbi export into a fresh folder", field.name))?,
                AxisSpec::Years => ensure!(col.integral() && col.min.as_deref().and_then(|s|s.parse::<u32>().ok()).is_some_and(|n|n>=1000)
                    && col.max.as_deref().and_then(|s|s.parse::<u32>().ok()).is_some_and(|n|n<=9999),
                    "{} no longer contains four-digit years; run powerbi export into a fresh folder", field.name),
                _ => {}
            }
        }
    }
    Ok(())
}

fn process_table(
    input: InputTable,
    chosen: bool,
    dest: &Path,
    writer: &mut SnapshotWriter,
    scratch: &resource::Scratch,
) -> Result<(Table, Profile)> {
    let InputTable {
        group,
        id,
        rules,
        saved,
    } = input;
    let incoming = headers(&group)?;
    let updating = saved.is_some();
    let mut table = saved.unwrap_or_else(|| Table {
        id,
        name: group.name.clone(),
        fields: vec![],
        axes: vec![],
        charts: vec![],
    });
    let order = if updating {
        let index: BTreeMap<_, _> = table
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| (f.key.as_str(), i))
            .collect();
        Some(
            incoming
                .iter()
                .map(|n| {
                    index
                        .get(header_key(n).as_str())
                        .copied()
                        .context("changed model field")
                })
                .collect::<Result<Vec<_>>>()?,
        )
    } else {
        None
    };
    let fields = if updating {
        table.fields.iter().map(|f| f.name.clone()).collect()
    } else {
        incoming
    };
    let inf = group.inference(fields.len(), order.as_deref())?;
    let kinds: Vec<_> = inf.kinds().collect();
    let mut profiler = Profiler::new(&fields, inf, scratch)?;
    profiler.enable_powerbi();
    let helpers = if updating {
        !table.axes.is_empty()
    } else {
        chosen
    };
    let writing_kinds: Vec<_> = if updating {
        table
            .fields
            .iter()
            .map(|f| {
                if matches!(f.kind, Type::Empty | Type::Text) {
                    Type::Text
                } else {
                    f.kind
                }
            })
            .collect()
    } else {
        kinds
    };
    let mut plain = if helpers {
        None
    } else {
        Some(FactWriter::plain(dest, &table.id, &writing_kinds)?)
    };
    for part in &group.parts {
        part.replay(order.as_deref(), |r| {
            profiler.observe(r, &part.source)?;
            if let Some(w) = &mut plain {
                w.row(r, &part.source)?;
            }
            Ok(())
        })?;
    }
    if let Some(w) = plain {
        w.finish()?;
    }
    let profile = profiler.finish()?;
    profile.columns.try_for_each(|i, c| {
        writer.profile(&table.name, c, profile.rows)?;
        if updating {
            update_column(&mut table, i, c)?;
        } else {
            table.fields.push(Field {
                name: c.name.clone(),
                key: header_key(&c.name),
                kind: c.kind,
                import: c.import_type()?,
                utc: c.utc(),
            });
        }
        Ok(())
    })?;
    let pending = if updating {
        None
    } else {
        Some(planner::plan(
            &profile,
            &mut table,
            &rules,
            chosen,
            writer.decisions(),
        )?)
    };
    let pairs = pending.as_ref().map(|p| p.pairs()).unwrap_or_default();
    let support = if helpers {
        let mut facts = FactWriter::new(dest, &table, &pairs)?;
        for part in group.parts {
            part.replay(order.as_deref(), |r| facts.row(r, &part.source))?;
        }
        facts.finish()?
    } else {
        Vec::new()
    };
    if let Some(p) = pending {
        p.finish(&mut table, &support, writer.decisions())?;
    }
    Ok((table, profile))
}

pub(crate) fn run(command: Command, mem: &resource::MemoryGuard) -> Result<bool> {
    let start = std::time::Instant::now();
    let (opts, charts, html, updating) = match command {
        Command::Export {
            input,
            charts,
            html,
        } => (input, charts, html, false),
        Command::UpdateData { input } => (input, None, None, true),
    };
    let cfg = config(&opts.common)?;
    let charts = ChartConfig::load(charts.as_deref()).context(InvalidSettings)?;
    let scratch = scratch(&opts.common, &cfg)?;
    let files = discover(&opts.inputs, opts.recursive, scratch.path())?;
    let dest = destination(&opts.output)?;
    let html = html.as_deref().map(destination).transpose()?;
    for file in &files {
        ensure!(
            !same_dest(&dest, &file.path)
                && !file.path.starts_with(&dest)
                && !html.as_ref().is_some_and(|p| same_dest(p, &file.path)),
            "input overlaps a Power BI output: {}",
            file.path.display()
        );
    }
    let stage = Stage::open(&dest, updating, html.as_deref())?;
    let baseline = if updating {
        Some(stage.load_contract()?)
    } else {
        None
    };
    let dataset = ingest(files, &cfg, &scratch, opts.quiet, &mut None)?;
    let (groups, chosen) = prepare(dataset.groups, &charts, baseline)?;
    let mut snapshot = SnapshotWriter::new(stage.snapshot())?;
    if updating {
        stage.copy_initial_decisions(snapshot.decisions())?;
    }
    let table_names: BTreeMap<_, _> = groups
        .iter()
        .filter_map(|g| {
            g.saved
                .as_ref()
                .map(|old| (g.group.name.clone(), old.name.clone()))
        })
        .collect();
    dataset.sources.try_for_each(|mut s| {
        if let Some(name) = s.table.as_ref().and_then(|n| table_names.get(n)) {
            s.table = Some(name.clone());
        }
        snapshot.source(&s)
    })?;
    let mut contract = Contract {
        version: 1,
        template_version: 1,
        tables: Vec::new(),
    };
    let mut sections = Vec::new();
    let mut html_profiles = Vec::new();
    for group in groups {
        scratch.check()?;
        if !opts.quiet {
            eprintln!(
                "Profiling {} ({} accepted rows)",
                group.group.name, group.group.accepted
            );
        }
        let selected = chosen.contains(&group.id);
        let (table, profile) =
            process_table(group, selected, stage.snapshot(), &mut snapshot, &scratch)?;
        if html.is_some() {
            let file = crate::store_json(&scratch, &profile)?;
            sections.push(report::TableSection {
                name: table.name.clone(),
                columns: table.fields.len(),
                rows: profile.rows,
                profile: file.path().into(),
                clean_file: None,
            });
            html_profiles.push(file);
        }
        contract.tables.push(table);
    }
    snapshot.finish()?;
    let staged_html = html
        .as_deref()
        .map(|p| -> Result<_> {
            let mut file = NamedTempFile::new_in(parent(p))?;
            let limit = usize::try_from(
                cfg.max_html_mib
                    .unwrap_or(32)
                    .checked_mul(1 << 20)
                    .context("HTML limit overflow")?,
            )?;
            let mut writer = BufWriter::new(VolumeWriter {
                inner: file.as_file_mut(),
                dir: parent(p).into(),
                until: 0,
            });
            report::write_report(&mut writer, &dataset.sources, &sections, limit)?;
            writer.flush()?;
            drop(writer);
            file.as_file().sync_all()?;
            Ok(file)
        })
        .transpose()?;
    scratch.check()?;
    let outcome = stage.finish(&contract, staged_html)?;
    if !opts.quiet {
        eprintln!(
            "{} accepted rows; peak scratch {} bytes; {}; {:.2}s; snapshot {} bytes",
            dataset.sources.rows,
            scratch.peak(),
            mem.stats(),
            start.elapsed().as_secs_f64(),
            outcome.bytes
        );
    }
    if updating {
        eprintln!(
            "Set DataDuckSnapshotFolder in Power BI Desktop to {} and select Refresh.",
            outcome.snapshot.display()
        );
    } else {
        eprintln!(
            "Open {} and select Refresh.",
            dest.join(format!(
                "{}.pbip",
                dest.file_name().unwrap_or_default().to_string_lossy()
            ))
            .display()
        );
    }
    eprintln!(
        "Snapshot: {}{}",
        outcome.snapshot.display(),
        if dataset.sources.partial {
            " (partial export)"
        } else {
            ""
        }
    );
    Ok(dataset.sources.partial)
}
