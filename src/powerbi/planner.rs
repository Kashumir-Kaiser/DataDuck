//! Business meaning is inferred only from a small exact-name dictionary or explicit rules.
use super::{Aggregate, Axis, AxisSpec, Chart, Table, Visual};
use crate::layout::header_key;
use crate::profile::{Column, ImportType, Profile, Type};
use anyhow::{bail, ensure, Context, Result};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

const MAX_CONFIG_BYTES: u64 = 2 << 20;
const MAX_VISUALS: usize = 8;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Identifier,
    Category,
    Year,
    EventDate,
    Numeric,
    Measure,
    ProfileOnly,
}

#[derive(Default, Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct FieldRule {
    pub role: Option<Role>,
    pub aggregate: Option<Aggregate>,
    #[serde(default)]
    pub priority: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selector {
    source: String,
    sheet: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TableOverride {
    #[serde(rename = "match")]
    selector: Selector,
    #[serde(default)]
    priority: u8,
    #[serde(default, deserialize_with = "read_fields")]
    fields: BTreeMap<String, FieldRule>,
}

// serde's normal map visitor accepts a repeated key by replacing the first value.
// Retain normalized header identity here so even exact duplicate JSON keys are caught.
fn read_fields<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, FieldRule>, D::Error> {
    struct Fields;
    impl<'de> Visitor<'de> for Fields {
        type Value = BTreeMap<String, FieldRule>;
        fn expecting(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt.write_str("an object keyed by field name")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut fields = BTreeMap::new();
            while let Some((name, rule)) = map.next_entry::<String, FieldRule>()? {
                let key = header_key(&name);
                if key.is_empty() || fields.insert(key, rule).is_some() {
                    return Err(de::Error::custom(format!(
                        "empty or duplicate chart field {name:?}"
                    )));
                }
            }
            Ok(fields)
        }
    }
    deserializer.deserialize_map(Fields)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChartConfig {
    version: u32,
    tables: Vec<TableOverride>,
}

#[derive(Default)]
pub(crate) struct TableRules {
    pub priority: u8,
    fields: BTreeMap<String, FieldRule>,
}

impl ChartConfig {
    pub(crate) fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self {
                version: 1,
                tables: vec![],
            });
        };
        let mut bytes = vec![];
        File::open(path)
            .with_context(|| format!("open chart config {}", path.display()))?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_CONFIG_BYTES,
            "chart config exceeds 2 MiB"
        );
        Self::parse(&bytes).with_context(|| format!("chart config {}", path.display()))
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        let c: Self = serde_json::from_slice(bytes)?;
        ensure!(
            c.version == 1,
            "unsupported chart config version {}",
            c.version
        );
        for table in &c.tables {
            ensure!(
                !table.selector.source.trim().is_empty(),
                "empty source selector"
            );
            ensure!(table.priority <= 100, "table priority exceeds 100");
            for (name, rule) in &table.fields {
                ensure!(rule.priority <= 100, "field {name:?} priority exceeds 100");
                ensure!(
                    rule.aggregate.is_none() || rule.role == Some(Role::Measure),
                    "field {name:?} with an aggregate must have role measure"
                );
            }
        }
        Ok(c)
    }

    pub(crate) fn resolve(
        &self,
        sources: &[(String, Option<String>)],
        headers: &[String],
        used: &mut BTreeSet<usize>,
    ) -> Result<TableRules> {
        let mut result = TableRules::default();
        for (index, entry) in self.tables.iter().enumerate() {
            let sel = &entry.selector;
            if !sources.iter().any(|(path, sheet)| {
                path == &sel.source
                    && sel
                        .sheet
                        .as_ref()
                        .is_none_or(|name| sheet.as_ref() == Some(name))
            }) {
                continue;
            }
            ensure!(
                used.insert(index),
                "chart selector for {:?} matches several logical tables; specify a sheet",
                sel.source
            );
            result.priority = result.priority.max(entry.priority);
            let known: BTreeSet<_> = headers.iter().map(|h| header_key(h)).collect();
            for (name, rule) in &entry.fields {
                ensure!(
                    known.contains(name),
                    "unknown chart field {name:?} in {:?}",
                    sel.source
                );
                if let Some(prev) = result.fields.insert(name.clone(), rule.clone()) {
                    ensure!(
                        &prev == rule,
                        "conflicting chart overrides for field {name:?}"
                    );
                }
            }
        }
        Ok(result)
    }

    pub(crate) fn check_all_matched(&self, used: &BTreeSet<usize>) -> Result<()> {
        if let Some((_, entry)) = self
            .tables
            .iter()
            .enumerate()
            .find(|(i, _)| !used.contains(i))
        {
            bail!(
                "chart selector did not match a usable source: {:?} sheet {:?}",
                entry.selector.source,
                entry.selector.sheet
            );
        }
        Ok(())
    }
}

// Break before a camel-case capital and before the last capital of an acronym
// followed by a lowercase letter: saleDate, VINCode, and order_ID all tokenize.
fn name_tokens(name: &str) -> Vec<String> {
    let mut chars = name.chars().peekable();
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut previous = None::<char>;
    while let Some(ch) = chars.next() {
        let separator = !ch.is_alphanumeric();
        let camel_boundary = ch.is_uppercase()
            && previous.is_some_and(|p| {
                p.is_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_uppercase() && chars.peek().is_some_and(|n| n.is_lowercase()))
            });
        if (separator || camel_boundary) && !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
        if !separator {
            token.extend(ch.to_lowercase());
        }
        previous = Some(ch);
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn identifier(tokens: &[String]) -> bool {
    tokens.iter().any(|t| {
        matches!(
            t.as_str(),
            "id" | "uuid" | "guid" | "vin" | "sku" | "code" | "key"
        )
    })
}
fn price(tokens: &[String]) -> bool {
    matches!(
        tokens.join("_").as_str(),
        "price" | "unit_price" | "selling_price" | "purchase_price"
    )
}
fn event_date(tokens: &[String]) -> bool {
    matches!(
        tokens.join("_").as_str(),
        "order_date" | "sale_date" | "transaction_date" | "event_date"
    )
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Treatment {
    Empty,
    Constant,
    ProfileOnly,
    Identifier,
    Binary,
    Year,
    SinglePeriod,
    EventDate,
    Date,
    Discrete,
    Histogram,
    Category,
    NearUnique,
    TopCategory,
}
impl Treatment {
    fn rule(self) -> &'static str {
        match self {
            Self::Empty => "all_missing",
            Self::Constant => "constant",
            Self::ProfileOnly => "profile_only",
            Self::Identifier => "identifier",
            Self::Binary => "binary_counts",
            Self::Year => "year_counts",
            Self::SinglePeriod => "one_populated_period",
            Self::EventDate => "event_count",
            Self::Date => "date_distribution",
            Self::Discrete => "discrete_counts",
            Self::Histogram => "histogram",
            Self::Category => "category_counts",
            Self::NearUnique => "near_unique_text",
            Self::TopCategory => "top_categories",
        }
    }
    fn order(self) -> Option<u8> {
        match self {
            Self::EventDate => Some(2),
            Self::Category | Self::TopCategory | Self::Binary => Some(3),
            Self::Histogram => Some(4),
            Self::Year | Self::Discrete => Some(5),
            Self::Date => Some(6),
            _ => None,
        }
    }
    fn dimension_order(self, distinct: u64) -> Option<u8> {
        match self {
            Self::Category | Self::Discrete | Self::Binary if distinct > 2 => Some(0),
            Self::Category | Self::Discrete | Self::Binary => Some(1),
            Self::TopCategory => Some(2),
            _ => None,
        }
    }
}
struct Evidence {
    treatment: Treatment,
    valid: u64,
    missing: u64,
    distinct: u64,
    periods: usize,
    priority: u8,
    role: Option<Role>,
    inferred: Type,
    imported: ImportType,
    integral: bool,
    aggregate: Option<Aggregate>,
}
fn year_domain(c: &Column) -> bool {
    let bound = |s: &Option<String>| s.as_deref().and_then(|s| s.parse::<u32>().ok());
    c.integral()
        && bound(&c.min).is_some_and(|v| v >= 1000)
        && bound(&c.max).is_some_and(|v| v <= 9999)
}
fn assess(c: &Column, import: ImportType, rule: &FieldRule) -> Result<Evidence> {
    let tokens = name_tokens(&c.name);
    let role = rule.role;
    let numeric = matches!(c.kind, Type::Integer | Type::Decimal);
    let import_numeric = matches!(
        import,
        ImportType::Whole | ImportType::Fixed | ImportType::Double
    );
    let date = matches!(c.kind, Type::Date | Type::Datetime);
    let valid_year = year_domain(c);
    let periods = if date {
        c.bins.iter().filter(|b| b.count > 0).count()
    } else {
        0
    };
    if c.valid > 0 {
        match role {
            Some(Role::Year) => ensure!(
                numeric && valid_year,
                "year requires integral values between 1000 and 9999"
            ),
            Some(Role::Numeric) => ensure!(numeric, "numeric requires a numeric column"),
            Some(Role::EventDate) => ensure!(date, "event_date requires a date or datetime column"),
            Some(Role::Measure) => ensure!(
                numeric && import_numeric,
                "measure requires a numeric column representable in Power BI"
            ),
            _ => {}
        }
    }
    let category = || {
        if c.distinct <= 12 {
            Treatment::Category
        } else {
            Treatment::TopCategory
        }
    };
    let treatment = if c.valid == 0 {
        Treatment::Empty
    } else if c.distinct == 1 {
        Treatment::Constant
    } else if role == Some(Role::ProfileOnly) {
        Treatment::ProfileOnly
    } else if role == Some(Role::Identifier) || (role.is_none() && identifier(&tokens)) {
        Treatment::Identifier
    } else if role == Some(Role::Category) {
        category()
    } else if matches!(role, Some(Role::Numeric | Role::Measure)) {
        Treatment::Histogram
    } else if c.kind == Type::Boolean
        || (numeric
            && c.distinct == 2
            && c.top.iter().any(|v| v.value == "0")
            && c.top.iter().any(|v| v.value == "1"))
    {
        Treatment::Binary
    } else if role == Some(Role::Year) || (tokens.last().is_some_and(|w| w == "year") && valid_year)
    {
        Treatment::Year
    } else if date && periods < 2 {
        Treatment::SinglePeriod
    } else if date && (role == Some(Role::EventDate) || event_date(&tokens)) {
        Treatment::EventDate
    } else if date {
        Treatment::Date
    } else if numeric && c.integral() && c.distinct <= 12 {
        Treatment::Discrete
    } else if numeric {
        Treatment::Histogram
    } else if c.distinct > 12
        && c.valid >= 20
        && u128::from(c.distinct) * 100 >= u128::from(c.valid) * 95
    {
        Treatment::NearUnique
    } else {
        category()
    };
    let aggregate = if role == Some(Role::Measure) {
        Some(
            rule.aggregate
                .or_else(|| price(&tokens).then_some(Aggregate::Average))
                .context("declare average, sum, min or max for this measure")?,
        )
    } else if role.is_none() && import_numeric && numeric && price(&tokens) {
        Some(Aggregate::Average)
    } else {
        None
    };
    Ok(Evidence {
        treatment,
        valid: c.valid,
        missing: c.missing,
        distinct: c.distinct,
        periods,
        priority: rule.priority,
        role,
        inferred: c.kind,
        imported: import,
        integral: c.integral(),
        aggregate,
    })
}

#[derive(Clone, Copy)]
struct Candidate {
    axis: usize,
    measure: Option<usize>,
    order: u8,
    pair: Option<usize>,
}
impl Candidate {
    fn compare(self, other: Self, evidence: &[Evidence], table: &Table) -> Ordering {
        let ranks = |c: Self| {
            let a = &evidence[c.axis];
            let m = c.measure.map(|i| &evidence[i]);
            let mp = m.map_or(0, |m| m.priority);
            (
                std::cmp::Reverse(a.priority.max(mp)),
                std::cmp::Reverse(a.priority.min(mp)),
                c.order,
                std::cmp::Reverse(
                    u8::from(a.role.is_some()) + u8::from(m.is_some_and(|m| m.role.is_some())),
                ),
                if c.order == 1 {
                    a.treatment.dimension_order(a.distinct).unwrap_or(u8::MAX)
                } else {
                    0
                },
                std::cmp::Reverse(m.map_or(a.valid, |m| a.valid.min(m.valid))),
            )
        };
        ranks(self)
            .cmp(&ranks(other))
            .then_with(|| {
                table.fields[self.axis]
                    .key
                    .cmp(&table.fields[other.axis].key)
            })
            .then_with(|| {
                self.measure
                    .map(|i| &table.fields[i].key)
                    .cmp(&other.measure.map(|i| &table.fields[i].key))
            })
    }
}
fn select(candidates: &[Candidate], allowed: impl Fn(Candidate) -> bool) -> Vec<usize> {
    let mut used_fields = BTreeSet::new();
    let mut selected = Vec::with_capacity(MAX_VISUALS);
    // Prefer distinct fields on the first page, then return to ranked ordering.
    for (i, c) in candidates
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, c)| allowed(*c))
    {
        if !used_fields.contains(&c.axis) && c.measure.is_none_or(|m| !used_fields.contains(&m)) {
            selected.push(i);
            used_fields.insert(c.axis);
            used_fields.extend(c.measure);
            if selected.len() == 4 {
                break;
            }
        }
    }
    for (i, _) in candidates
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, c)| allowed(*c))
    {
        // If no new fields fit the first page, fill its remaining slots in
        // rank order. The second page is reserved for single-field distributions.
        if !selected.contains(&i) && (selected.len() < 4 || candidates[i].measure.is_none()) {
            selected.push(i);
        }
        if selected.len() == MAX_VISUALS {
            break;
        }
    }
    selected
}
/// Maintain a bounded shortlist while scanning arbitrarily wide tables.
fn keep_best<T>(items: &mut Vec<T>, cap: usize, item: T, cmp: impl Fn(&T, &T) -> Ordering) {
    let pos = items.partition_point(|other| cmp(other, &item).is_le());
    if pos < cap {
        if items.len() == cap {
            items.pop();
        }
        items.insert(pos, item);
    }
}

pub(crate) struct Pending {
    fields: Vec<Evidence>,
    candidates: Vec<Candidate>,
    pairs: Vec<(usize, usize)>,
}
fn field_record(
    w: &mut impl Write,
    table: &Table,
    field: usize,
    e: &Evidence,
    status: &str,
) -> Result<()> {
    serde_json::to_writer(
        &mut *w,
        &serde_json::json!({
            "table":table.id,"field":table.fields[field].name,"field_index":field,
            "rule":e.treatment.rule(),"valid":e.valid,"missing":e.missing,"distinct":e.distinct,
            "aggregation":e.aggregate,"status":status,
            "inferred_type":e.inferred,"model_type":e.imported,"integral":e.integral,
            "declared_role":e.role,"priority":e.priority,"populated_periods":e.periods,
            "reason":match status {
                "table_budget" => "logical table outside the analysis page budget",
                "page_budget" => "eligible field below the visual selection cutoff",
                "selected_axis" => "selected as a chart axis",
                "selected_measure" => "selected as an analytical measure",
                _ => "single-column rule retained this field in the profile"
            }
        }),
    )?;
    w.write_all(b"\n")?;
    Ok(())
}

pub(crate) fn plan(
    p: &Profile,
    table: &mut Table,
    rules: &TableRules,
    selected_table: bool,
    decisions: &mut impl Write,
) -> Result<Pending> {
    let mut fields = Vec::new();
    p.columns.try_for_each(|i, c| {
        let field = table
            .fields
            .get(i)
            .context("missing field in Power BI table")?;
        let default = FieldRule::default();
        let rule = rules.fields.get(&header_key(&c.name)).unwrap_or(&default);
        let evidence = assess(c, field.import, rule)
            .with_context(|| format!("chart field {:?}", c.name))
            .context(crate::InvalidSettings)?;
        if selected_table {
            fields.push(evidence);
        } else {
            field_record(
                decisions,
                table,
                i,
                &evidence,
                if evidence.treatment.order().is_some() {
                    "table_budget"
                } else {
                    "profile_only"
                },
            )?;
        }
        Ok(())
    })?;
    let mut result = Pending {
        fields,
        candidates: Vec::new(),
        pairs: Vec::new(),
    };
    if !selected_table {
        return Ok(result);
    }
    let evidence = &result.fields;
    ensure!(
        evidence.len() == table.fields.len(),
        "column count mismatch"
    );
    let rank_field = |a: usize, b: usize| {
        let (a_info, b_info) = (&evidence[a], &evidence[b]);
        b_info
            .priority
            .cmp(&a_info.priority)
            .then(b_info.role.is_some().cmp(&a_info.role.is_some()))
    };
    let tie = |a: usize, b: usize| {
        evidence[b]
            .valid
            .cmp(&evidence[a].valid)
            .then_with(|| table.fields[a].key.cmp(&table.fields[b].key))
    };
    let mut dims = Vec::with_capacity(2);
    let mut measures = Vec::with_capacity(2);
    // At most five fields from the shortlisted pairs can conflict with
    // the distinct-field preference on page one.
    let mut candidates = Vec::with_capacity(MAX_VISUALS + 5 + 6);
    for (field, info) in evidence.iter().enumerate() {
        let Some(order) = info.treatment.order() else {
            continue;
        };
        keep_best(
            &mut candidates,
            MAX_VISUALS + 5,
            Candidate {
                axis: field,
                measure: None,
                order,
                pair: None,
            },
            |a, b| a.compare(*b, evidence, table),
        );
        if info.aggregate.is_some() {
            keep_best(&mut measures, 2, field, |&a, &b| {
                rank_field(a, b).then_with(|| tie(a, b))
            });
        } else if let Some(dimension) = info.treatment.dimension_order(info.distinct) {
            keep_best(&mut dims, 2, (field, dimension), |&(a, ad), &(b, bd)| {
                rank_field(a, b).then(ad.cmp(&bd)).then_with(|| tie(a, b))
            });
        }
    }
    let date = (0..evidence.len())
        .filter(|&i| evidence[i].treatment == Treatment::EventDate)
        .min_by(|&a, &b| rank_field(a, b).then_with(|| tie(a, b)));
    for m in measures {
        for (a, order) in date
            .into_iter()
            .map(|a| (a, 0))
            .chain(dims.iter().map(|&(a, _)| (a, 1)))
        {
            if a == m {
                continue;
            }
            candidates.push(Candidate {
                axis: a,
                measure: Some(m),
                order,
                pair: Some(result.pairs.len()),
            });
            result.pairs.push((a, m));
        }
    }
    candidates.sort_by(|a, b| a.compare(*b, evidence, table));
    // Only axes which could bind a visual need row keys; pair axes also supply the
    // group-support check. Six possible pairs means at most 64 small masks.
    let mut axes: BTreeSet<_> = result.pairs.iter().map(|(a, _)| *a).collect();
    for mask in 0..(1usize << result.pairs.len()) {
        for i in select(&candidates, |c| c.pair.is_none_or(|n| mask & (1 << n) != 0)) {
            axes.insert(candidates[i].axis);
        }
    }
    p.columns.try_for_each(|i, c| {
        if axes.contains(&i) {
            let spec = axis_spec(evidence[i].treatment, c)?;
            table.axes.push(Axis { field: i, spec });
        }
        Ok(())
    })?;
    result.candidates = candidates;
    Ok(result)
}
fn axis_spec(t: Treatment, c: &Column) -> Result<AxisSpec> {
    Ok(match t {
        Treatment::Year => AxisSpec::Years,
        Treatment::Date | Treatment::EventDate => AxisSpec::Calendar(c.calendar()?),
        Treatment::Histogram => AxisSpec::Histogram(c.histogram()?),
        Treatment::Discrete => {
            let mut vals: Vec<_> = c.top.iter().map(|v| v.value.clone()).collect();
            // Insertion sort on at most 12 values permits fallible exact decimal comparison.
            for i in 1..vals.len() {
                let mut j = i;
                while j > 0 && crate::profile::compare_numbers(&vals[j], &vals[j - 1])?.is_lt() {
                    vals.swap(j, j - 1);
                    j -= 1;
                }
            }
            AxisSpec::Discrete(vals)
        }
        Treatment::Category | Treatment::Binary | Treatment::TopCategory => {
            let take = if t == Treatment::TopCategory { 10 } else { 12 };
            AxisSpec::Categories(c.top.iter().take(take).map(|r| r.value.clone()).collect())
        }
        _ => bail!("non-chart field selected as axis"),
    })
}
impl Pending {
    pub(crate) fn pairs(&self) -> Vec<(usize, usize)> {
        self.pairs.clone()
    }
    pub(crate) fn finish(
        self,
        table: &mut Table,
        support: &[(u64, usize)],
        decisions: &mut impl Write,
    ) -> Result<()> {
        ensure!(
            support.len() == self.pairs.len(),
            "missing chart pair support"
        );
        let supported = |c: Candidate| {
            c.pair
                .is_none_or(|i| support[i].0 >= 10 && support[i].1 >= 2)
        };
        let selected = select(&self.candidates, supported);
        let mut axes = BTreeSet::new();
        let mut measures = BTreeSet::new();
        table.charts.clear();
        for &index in &selected {
            let c = self.candidates[index];
            axes.insert(c.axis);
            measures.extend(c.measure);
            table.charts.push(self.chart(c, table, support));
        }
        for (i, e) in self.fields.iter().enumerate() {
            let status = if axes.contains(&i) {
                "selected_axis"
            } else if measures.contains(&i) {
                "selected_measure"
            } else if e.treatment.order().is_some() {
                "page_budget"
            } else {
                "profile_only"
            };
            field_record(decisions, table, i, e, status)?;
        }
        for (i, c) in self.candidates.iter().enumerate() {
            if let (Some(pair), Some(m)) = (c.pair, c.measure) {
                let status = if !supported(*c) {
                    "insufficient_pair_support"
                } else if selected.contains(&i) {
                    "selected"
                } else {
                    "page_budget"
                };
                let chart = self.chart(*c, table, support);
                serde_json::to_writer(
                    &mut *decisions,
                    &serde_json::json!({
                        "table":table.id,"axis":table.fields[c.axis].name,"measure":table.fields[m].name,
                        "rule":chart.rule,"aggregation":chart.aggregate,"valid_pairs":support[pair].0,
                        "groups_at_least_two":support[pair].1>=2,"populated_groups_capped_at_three":support[pair].1,
                        "status":status,"reason":match status {
                            "insufficient_pair_support" => "requires ten jointly valid rows and at least two populated groups",
                            "selected" => "selected from ranked pairs with sufficient joint support",
                            _ => "eligible pair below the visual selection cutoff"
                        }
                    }),
                )?;
                decisions.write_all(b"\n")?;
            }
        }
        Ok(())
    }
    fn chart(&self, c: Candidate, table: &Table, support: &[(u64, usize)]) -> Chart {
        let e = &self.fields[c.axis];
        let axis_name = table.fields[c.axis].name.replace('_', " ");
        let aggregate = c.measure.and_then(|i| self.fields[i].aggregate);
        let human = |s: &str| s.replace('_', " ").to_lowercase();
        let title = if let (Some(m), Some(agg)) = (c.measure, aggregate) {
            let agg = match agg {
                Aggregate::Average => "Average",
                Aggregate::Sum => "Sum of",
                Aggregate::Min => "Minimum",
                Aggregate::Max => "Maximum",
            };
            format!(
                "{agg} {} by {}",
                human(&table.fields[m].name),
                human(&axis_name)
            )
        } else if e.treatment == Treatment::Histogram {
            format!("{axis_name} distribution")
        } else {
            format!("Records by {}", human(&axis_name))
        };
        let (visual, rule) = if c.measure.is_some() && c.order == 1 {
            (Visual::Bars, "measure_by_category")
        } else if c.measure.is_some() {
            (
                if c.pair.is_some_and(|i| support[i].1 >= 3) {
                    Visual::Line
                } else {
                    Visual::Columns
                },
                "measure_by_event_date",
            )
        } else {
            (
                match e.treatment {
                    Treatment::Date | Treatment::EventDate if e.periods >= 3 => Visual::Line,
                    Treatment::Category | Treatment::TopCategory | Treatment::Binary => {
                        Visual::Bars
                    }
                    _ => Visual::Columns,
                },
                e.treatment.rule(),
            )
        };
        Chart {
            axis: c.axis,
            measure: c.measure,
            aggregate,
            visual,
            title,
            rule: rule.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell, Kind, Row};
    use crate::powerbi::Field;
    use crate::profile::{Inference, Profiler};
    use crate::resource::Scratch;
    use serde_json::{json, Value};

    fn evidence(name: &str, kind: &str, valid: u64, distinct: u64, integral: bool) -> Column {
        serde_json::from_value(json!({
            "name":name,"kind":kind,"valid":valid,"missing":1000-valid,"distinct":distinct,
            "top":[],"other":0,"min":null,"max":null,"mean":null,"bins":[],"missing_reasons":{},
            "evidence":{"integral":integral,"whole":integral,"fixed":true,"double":true,
                "calendar":true,"zoned":false,"naive":false}
        }))
        .unwrap()
    }

    #[test]
    fn decision_boundaries_use_whole_column_counts_and_exact_header_tokens() -> Result<()> {
        let cases = [
            ("empty", "empty", 0, 0, false, Treatment::Empty),
            ("id", "text", 10, 1, false, Treatment::Constant),
            (
                "VehicleVINCode",
                "text",
                100,
                11,
                false,
                Treatment::Identifier,
            ),
            ("paid", "integer", 100, 98, true, Treatment::Histogram),
            (
                "Vehicle_Category",
                "text",
                100,
                12,
                false,
                Treatment::Category,
            ),
            (
                "Vehicle_Category",
                "text",
                100,
                13,
                false,
                Treatment::TopCategory,
            ),
            ("Comment", "text", 19, 19, false, Treatment::TopCategory),
            ("Comment", "text", 20, 19, false, Treatment::NearUnique),
            ("Comment", "text", 1000, 949, false, Treatment::TopCategory),
            ("Comment", "text", 1000, 950, false, Treatment::NearUnique),
            ("Owners", "decimal", 30, 12, true, Treatment::Discrete),
            ("Owners", "decimal", 30, 13, true, Treatment::Histogram),
            ("Engine_Size", "decimal", 30, 5, false, Treatment::Histogram),
            ("Mileage", "integer", 100, 99, true, Treatment::Histogram),
            (
                "AccidentHistory",
                "boolean",
                100,
                2,
                false,
                Treatment::Binary,
            ),
        ];
        for (name, kind, v, d, integral, expected) in cases {
            let c = evidence(name, kind, v, d, integral);
            assert_eq!(
                assess(&c, c.import_type()?, &FieldRule::default())?.treatment,
                expected,
                "{name} {v} {d}"
            );
        }
        let mut c = evidence("ModelYear", "decimal", 40, 20, true);
        c.min = Some("2005".into());
        c.max = Some("2024".into());
        assert_eq!(
            assess(&c, c.import_type()?, &FieldRule::default())?.treatment,
            Treatment::Year
        );
        c.min = Some("999".into());
        assert_eq!(
            assess(&c, c.import_type()?, &FieldRule::default())?.treatment,
            Treatment::Histogram
        );
        let c = evidence("sellingPrice", "decimal", 100, 100, false);
        assert_eq!(
            assess(&c, ImportType::Text, &FieldRule::default())?.aggregate,
            None
        );
        assert_eq!(
            assess(&c, ImportType::Fixed, &FieldRule::default())?.aggregate,
            Some(Aggregate::Average)
        );
        Ok(())
    }

    #[test]
    fn overrides_are_strict_and_match_logical_tables() -> Result<()> {
        let unrelated = [("other.xlsx".to_owned(), Some("Cars".into()))];
        let source = [("cars.xlsx".to_owned(), Some("Cars".into()))];
        let config = ChartConfig::parse(br#"{"version":1,"tables":[{"match":{"source":"cars.xlsx","sheet":"Cars"},"fields":{"Selling_Price":{"role":"measure","aggregate":"average","priority":100}}}]}"#)?;
        let headers = vec!["Selling_Price".to_owned()];
        let mut used = BTreeSet::new();
        assert!(config
            .resolve(&unrelated, &headers, &mut used)?
            .fields
            .is_empty());
        assert!(config.check_all_matched(&used).is_err());
        let resolved = config.resolve(&source, &headers, &mut used)?;
        assert_eq!(
            resolved.fields["selling_price"].aggregate,
            Some(Aggregate::Average)
        );
        config.check_all_matched(&used)?;
        assert!(config.resolve(&source, &headers, &mut used).is_err());
        assert!(ChartConfig::parse(
            br#"{"version":1,"tables":[{"match":{"source":"x"},"fields":{"ID":{},"id":{}}}]}"#
        )
        .is_err());
        assert!(ChartConfig::parse(br#"{"version":1,"tables":[],"unused":0}"#).is_err());
        assert!(ChartConfig::parse(br#"{"version":1,"tables":[{"match":{"source":"x"},"fields":{"price":{"role":"numeric","aggregate":"sum"}}}]}"#).is_err());
        Ok(())
    }

    fn cars() -> Result<(Profile, Table, TableRules)> {
        let names: Vec<String> = [
            "Make",
            "Selling_Price",
            "Engine_Size",
            "Year",
            "Order_Date",
            "OtherID",
            "Flag",
            "Fuel",
            "Country",
            "Sales",
            "Style",
            "Grade",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let mut rows = Vec::new();
        let mut inference = Inference::new(names.len());
        for i in 0..50 {
            let vals = [
                (Kind::Text, ["Ford", "M", "H"][i % 3].to_string()),
                (Kind::Number, (100 + i * 10).to_string()),
                (Kind::Number, format!("{}.1", i % 24)),
                (Kind::Number, format!("{}.0", 2005 + i % 20)),
                (Kind::Date, format!("2024-01-0{}", 1 + i % 3)),
                (Kind::Text, format!("00{i}")),
                (Kind::Number, (i % 2).to_string()),
                (Kind::Text, format!("fuel{}", i % 4)),
                (Kind::Text, format!("country{}", i % 7)),
                (Kind::Number, (i * 20).to_string()),
                (Kind::Text, format!("style{}", i % 4)),
                (Kind::Text, format!("grade{}", i % 4)),
            ];
            let cells = vals
                .into_iter()
                .enumerate()
                .map(|(col, (kind, value))| Cell { col, kind, value })
                .collect();
            let row = Row {
                number: i as u64 + 1,
                width: names.len(),
                cells,
                ..Default::default()
            };
            inference.observe(&row)?;
            rows.push(row);
        }
        let scratch = Scratch::new(None, 64 << 20)?;
        let mut profiler = Profiler::new(&names, inference, &scratch)?;
        profiler.enable_powerbi();
        for r in rows {
            profiler.observe(&r, "cars.xlsx / Cars")?;
        }
        let profile = profiler.finish()?;
        let mut table = Table {
            id: "cars".into(),
            name: "Cars".into(),
            fields: vec![],
            axes: vec![],
            charts: vec![],
        };
        profile.columns.try_for_each(|_, c| {
            table.fields.push(Field {
                name: c.name.clone(),
                key: header_key(&c.name),
                kind: c.kind,
                import: c.import_type()?,
                utc: c.utc(),
            });
            Ok(())
        })?;
        let config = ChartConfig::parse(br#"{"version":1,"tables":[{"match":{"source":"cars.xlsx"},"fields":{
            "Make":{"role":"category","priority":100},"Selling_Price":{"role":"measure","aggregate":"average","priority":100},
            "Engine_Size":{"role":"numeric","priority":90},"Sales":{"role":"measure","aggregate":"sum"}
        }}]}"#)?;
        let rules = config.resolve(&[("cars.xlsx".into(), None)], &names, &mut BTreeSet::new())?;
        Ok((profile, table, rules))
    }
    fn run_cars(
        profile: &Profile,
        mut table: Table,
        rules: &TableRules,
        valid: u64,
        groups: usize,
    ) -> Result<(Table, Vec<Value>)> {
        let mut audit = Vec::new();
        let pending = plan(profile, &mut table, rules, true, &mut audit)?;
        let support: Vec<_> = pending.pairs().iter().map(|_| (valid, groups)).collect();
        pending.finish(&mut table, &support, &mut audit)?;
        let log = serde_json::Deserializer::from_slice(&audit)
            .into_iter::<Value>()
            .collect::<std::result::Result<_, _>>()?;
        Ok((table, log))
    }

    #[test]
    fn supported_pairs_fallback_layout_order_and_actual_numeric_domains() -> Result<()> {
        let (profile, table, rules) = cars()?;
        let (result, records) = run_cars(&profile, table.clone(), &rules, 10, 2)?;
        let field = |n: &str| records.iter().find(|r| r["field"] == n).unwrap();
        assert_eq!(field("Flag")["rule"], "binary_counts");
        assert_eq!(field("Year")["rule"], "year_counts");
        assert_eq!(field("Engine_Size")["rule"], "histogram");
        assert_eq!(field("OtherID")["rule"], "identifier");
        assert_eq!(
            records.iter().filter(|r| r["field"].is_string()).count(),
            12
        );
        assert!(result
            .charts
            .iter()
            .any(|c| c.title == "Average selling price by make"));
        assert!(result
            .charts
            .iter()
            .any(|c| c.axis == 2 && c.rule == "histogram"));
        assert!(result
            .charts
            .iter()
            .any(|c| c.rule == "measure_by_event_date" && c.visual == Visual::Columns));
        assert_eq!(result.charts.len(), 8);
        assert!(result.charts[4..].iter().all(|c| c.measure.is_none()));
        assert_eq!(
            serde_json::to_value(&result)?,
            serde_json::to_value(run_cars(&profile, table.clone(), &rules, 10, 2)?.0)?
        );
        let (three, _) = run_cars(&profile, table.clone(), &rules, 10, 3)?;
        assert!(three
            .charts
            .iter()
            .any(|c| c.rule == "measure_by_event_date" && c.visual == Visual::Line));
        for (valid, groups) in [(9, 3), (10, 1)] {
            let (fallback, log) = run_cars(&profile, table.clone(), &rules, valid, groups)?;
            assert!(fallback.charts.iter().all(|c| c.measure.is_none()));
            assert!(log
                .iter()
                .any(|r| r["status"] == "insufficient_pair_support"));
        }
        Ok(())
    }
}
