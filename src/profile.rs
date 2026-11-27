//! Full-column inference and exact frequency profiles. Only the active column's
//! ranked values and bins reside in memory during the final reduction.
use crate::model::{Cell, Kind, Row, MAX_COLS};
use crate::resource::{check_cancel, Scratch, ScratchFile};
use crate::sort::Sorter;
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use num_bigint::BigInt;
use serde::{ser::SerializeSeq, Deserialize, Serialize, Serializer};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Write};

const TOP: usize = 20;
const HIST: usize = 32;
const TIME_BINS: usize = 512;
const SAMPLE_MAX: usize = 100;
const SAMPLE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Type {
    #[default]
    Empty,
    Text,
    Boolean,
    Integer,
    Decimal,
    Date,
    Datetime,
}

/// Canonical finite base-10 number, digits × 10^power. No trailing or
/// leading zero in a nonzero magnitude. Its ordering never passes via f64.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Decimal {
    negative: bool,
    digits: String,
    power: i32,
}
impl Decimal {
    fn new(negative: bool, digits: &str, power: i32) -> Self {
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            return Self {
                negative: false,
                digits: "0".into(),
                power: 0,
            };
        }
        let trimmed = digits.trim_end_matches('0');
        Self {
            negative,
            digits: trimmed.into(),
            power: power + (digits.len() - trimmed.len()) as i32,
        }
    }
    fn parse(raw: &str, preserve_id: bool) -> Option<(Self, bool)> {
        Self::parse_with_limit(raw, preserve_id, 512)
    }
    fn parse_with_limit(raw: &str, preserve_id: bool, exp_limit: i32) -> Option<(Self, bool)> {
        // This limit bounds both decimal arithmetic and retained extrema per column.
        if raw.len() > 144 || raw.is_empty() {
            return None;
        }
        let (negative, unsigned) = if let Some(s) = raw.strip_prefix('-') {
            (true, s)
        } else {
            (false, raw.strip_prefix('+').unwrap_or(raw))
        };
        let (mantissa, exponent) =
            unsigned
                .split_once(['e', 'E'])
                .map_or(Some((unsigned, 0)), |(m, e)| {
                    e.parse::<i32>()
                        .ok()
                        .filter(|v| (-exp_limit..=exp_limit).contains(v))
                        .map(|v| (m, v))
                })?;
        let (whole, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let ndigits = whole.len() + frac.len();
        if !(1..=128).contains(&ndigits)
            || !whole
                .bytes()
                .chain(frac.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        if preserve_id
            && ((whole.starts_with('0') && whole.len() > 1)
                || (mantissa == unsigned && !mantissa.contains('.') && whole.len() > 15))
        {
            return None;
        }
        let value = if frac.is_empty() {
            Self::new(negative, whole, exponent)
        } else {
            let mut digits = String::with_capacity(ndigits);
            digits.push_str(whole);
            digits.push_str(frac);
            Self::new(negative, &digits, exponent - frac.len() as i32)
        };
        let integer = !unsigned.contains(['.', 'e', 'E']);
        Some((value, integer))
    }
    fn label(&self) -> String {
        let point = self.digits.len() as i32 + self.power;
        let mut value = if self.negative {
            String::from("-")
        } else {
            String::new()
        };
        if self.power == 0 {
            value.push_str(&self.digits);
        } else if (1..=21).contains(&point) {
            if point as usize >= self.digits.len() {
                value.push_str(&self.digits);
                value.extend(std::iter::repeat_n('0', point as usize - self.digits.len()));
            } else {
                let (a, b) = self.digits.split_at(point as usize);
                value.push_str(a);
                value.push('.');
                value.push_str(b);
            }
        } else if (-5..=0).contains(&point) {
            value.push_str("0.");
            value.extend(std::iter::repeat_n('0', (-point) as usize));
            value.push_str(&self.digits);
        } else {
            value.push(self.digits.as_bytes()[0] as char);
            if self.digits.len() > 1 {
                value.push('.');
                value.push_str(&self.digits[1..]);
            }
            value.push('e');
            value.push_str(&(point - 1).to_string());
        }
        value
    }
    fn at_power(&self, power: i32) -> BigInt {
        let mut mag = BigInt::parse_bytes(self.digits.as_bytes(), 10).expect("validated digits");
        mag *= BigInt::from(10u8).pow((self.power - power) as u32);
        if self.negative {
            -mag
        } else {
            mag
        }
    }
}
impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.negative != other.negative {
            return other.negative.cmp(&self.negative);
        }
        let o = if self.digits == "0" || other.digits == "0" {
            (self.digits != "0").cmp(&(other.digits != "0"))
        } else {
            (self.digits.len() as i32 + self.power)
                .cmp(&(other.digits.len() as i32 + other.power))
                .then_with(|| self.digits.cmp(&other.digits))
        };
        if self.negative {
            o.reverse()
        } else {
            o
        }
    }
}
impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
struct Temporal {
    dt: NaiveDateTime,
    form: u8,
}
impl Temporal {
    fn parse(s: &str) -> Option<Self> {
        if s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
            return Some(Self {
                dt: NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .ok()?
                    .and_hms_opt(0, 0, 0)?,
                form: 0,
            });
        }
        if !(19..=40).contains(&s.len()) || !s.contains(['T', ' ']) {
            return None;
        }
        // Chrono truncates fractional precision above nanoseconds; retain those as text.
        if s.split_once('.')
            .is_some_and(|(_, f)| f.bytes().take_while(|b| b.is_ascii_digit()).count() > 9)
        {
            return None;
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(Self {
                dt: dt.naive_utc(),
                form: 2,
            });
        }
        // Crossing a year boundary during UTC normalization can produce a signed
        // extended year; chrono's strict RFC3339 parser accepts four-digit years.
        if let Some(utc) = s.strip_suffix('Z') {
            let dt = NaiveDateTime::parse_from_str(utc, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
            return Some(Self { dt, form: 2 });
        }
        let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
            .ok()?;
        Some(Self { dt, form: 1 })
    }
    fn label(&self) -> String {
        match self.form {
            0 => self.dt.date().to_string(),
            1 => self.dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
            _ => DateTime::<Utc>::from_naive_utc_and_offset(self.dt, Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true),
        }
    }
}

/// Accepted exact numeric representation for an explicitly numeric column.
pub fn supports_number(raw: &str) -> bool {
    Decimal::parse(raw, false).is_some()
}

enum Value {
    Text,
    Bool(bool),
    Number(Decimal, bool),
    Date(Temporal),
}
fn classify(cell: &Cell) -> Value {
    let s = cell.value.as_str();
    if cell.kind == Kind::Text {
        return Value::Text;
    }
    if matches!(cell.kind, Kind::Auto | Kind::Bool) {
        if s.eq_ignore_ascii_case("true") || (cell.kind == Kind::Bool && s == "1") {
            return Value::Bool(true);
        }
        if s.eq_ignore_ascii_case("false") || (cell.kind == Kind::Bool && s == "0") {
            return Value::Bool(false);
        }
    }
    if matches!(cell.kind, Kind::Auto | Kind::Number) {
        if let Some((n, i)) = Decimal::parse(s, cell.kind == Kind::Auto) {
            return Value::Number(n, i);
        }
    }
    if matches!(cell.kind, Kind::Auto | Kind::Date) {
        if let Some(d) = Temporal::parse(s) {
            return Value::Date(d);
        }
    }
    Value::Text
}

fn promote(a: Type, b: Type) -> Type {
    use Type::*;
    match (a, b) {
        (Empty, x) | (x, Empty) => x,
        (Integer, Decimal) | (Decimal, Integer) => Decimal,
        (x, y) if x == y => x,
        _ => Text,
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ColumnType {
    kind: Type,
    numbers: Option<(Decimal, Decimal)>,
    dates: Option<(Temporal, Temporal)>,
}
fn extend_range<T: Ord + Clone>(range: &mut Option<(T, T)>, value: &T) {
    match range {
        Some((lo, hi)) => {
            if value < lo {
                lo.clone_from(value);
            }
            if value > hi {
                hi.clone_from(value);
            }
        }
        None => *range = Some((value.clone(), value.clone())),
    }
}
impl ColumnType {
    fn ingest(&mut self, v: Value) {
        let kind = match &v {
            Value::Text => Type::Text,
            Value::Bool(_) => Type::Boolean,
            Value::Number(_, true) => Type::Integer,
            Value::Number(_, false) => Type::Decimal,
            Value::Date(d) if d.form == 0 => Type::Date,
            Value::Date(_) => Type::Datetime,
        };
        self.kind = promote(self.kind, kind);
        if self.kind == Type::Text {
            self.numbers = None;
            self.dates = None;
            return;
        }
        match v {
            Value::Number(x, _) => extend_range(&mut self.numbers, &x),
            Value::Date(x) => extend_range(&mut self.dates, &x),
            _ => {}
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Inference {
    types: Vec<ColumnType>,
}
impl Inference {
    pub fn new(width: usize) -> Self {
        Self {
            types: vec![ColumnType::default(); width],
        }
    }
    pub fn observe(&mut self, row: &Row) -> Result<()> {
        for c in &row.cells {
            let t = self
                .types
                .get_mut(c.col)
                .context("cell outside table during inference")?;
            if c.kind != Kind::Missing && t.kind != Type::Text {
                t.ingest(classify(c));
            }
        }
        Ok(())
    }
    #[allow(dead_code)]
    pub fn merge(&mut self, other: &Self) -> Result<()> {
        ensure!(
            self.types.len() == other.types.len(),
            "cannot merge inferences of different widths"
        );
        self.merge_mapped(other, &(0..other.types.len()).collect::<Vec<_>>())
    }
    pub fn merge_mapped(&mut self, other: &Self, map: &[usize]) -> Result<()> {
        ensure!(
            map.len() == other.types.len(),
            "inference mapping length mismatch"
        );
        for (&dst, b) in map.iter().zip(&other.types) {
            let a = self
                .types
                .get_mut(dst)
                .context("invalid inference mapping")?;
            a.kind = promote(a.kind, b.kind);
            if a.kind == Type::Text {
                a.numbers = None;
                a.dates = None;
                continue;
            }
            if let Some((lo, hi)) = &b.numbers {
                extend_range(&mut a.numbers, lo);
                extend_range(&mut a.numbers, hi);
            }
            if let Some((lo, hi)) = &b.dates {
                extend_range(&mut a.dates, lo);
                extend_range(&mut a.dates, hi);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Ranked {
    value: String,
    count: u64,
}
#[derive(Debug, Serialize, Deserialize)]
struct Bin {
    label: String,
    count: u64,
}
#[derive(Serialize, Deserialize)]
struct Column {
    name: String,
    kind: Type,
    valid: u64,
    missing: u64,
    distinct: u64,
    top: Vec<Ranked>,
    other: u64,
    min: Option<String>,
    max: Option<String>,
    mean: Option<f64>,
    bins: Vec<Bin>,
    missing_reasons: BTreeMap<String, u64>,
}
#[derive(Serialize)]
pub struct Sample {
    source: String,
    row: u64,
    values: Vec<Option<String>>,
}
/// Sequential column objects, each separated by JSON whitespace. Reopening on
/// serialization permits cap estimation and rendering without keeping a wide report in RAM.
pub struct Columns {
    file: ScratchFile,
    len: usize,
}
impl Serialize for Columns {
    fn serialize<S: Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let reader = BufReader::new(self.file.reopen().map_err(S::Error::custom)?);
        let mut seq = ser.serialize_seq(Some(self.len))?;
        let mut n = 0;
        for value in serde_json::Deserializer::from_reader(reader).into_iter::<Column>() {
            seq.serialize_element(&value.map_err(S::Error::custom)?)?;
            n += 1;
        }
        if n != self.len {
            return Err(S::Error::custom("incomplete column summary"));
        }
        seq.end()
    }
}
#[derive(Serialize)]
pub struct Profile {
    pub rows: u64,
    pub columns: Columns,
    pub sample: Vec<Sample>,
}

#[derive(Default)]
struct Tally {
    valid: u64,
    reasons: BTreeMap<String, u64>,
}
pub struct Profiler {
    headers: Vec<String>,
    inference: Inference,
    sorter: Sorter,
    scratch: Scratch,
    rows: u64,
    tallies: Vec<Tally>,
    sample: Vec<Sample>,
    sample_cap: usize,
    preview: usize,
}
fn clipped(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.into();
    }
    let mut end = max.saturating_sub(3).min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = String::from(&value[..end]);
    s.push('…');
    s
}
fn sample_slot(index: u64, capacity: usize) -> Option<usize> {
    if index <= capacity as u64 {
        return Some(index as usize - 1);
    }
    // Deterministic Algorithm R. The sequence depends on the stable input row order.
    let mut n = index.wrapping_add(0x9e3779b97f4a7c15);
    n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
    n ^= n >> 31;
    let j = ((u128::from(n) * u128::from(index)) >> 64) as u64;
    (j < capacity as u64).then_some(j as usize)
}
impl Profiler {
    pub fn new(headers: &[String], inference: Inference, scratch: &Scratch) -> Result<Self> {
        ensure!(
            headers.len() <= MAX_COLS && headers.len() == inference.types.len(),
            "table width mismatch"
        );
        let width = headers.len().max(1);
        let preview = (SAMPLE_BYTES / width).saturating_sub(64).clamp(4, 128);
        let sample_cap = (SAMPLE_BYTES / (4096 + width * (preview + 64))).clamp(1, SAMPLE_MAX);
        Ok(Self {
            headers: headers.to_vec(),
            inference,
            sorter: Sorter::new(scratch)?,
            scratch: scratch.clone(),
            rows: 0,
            tallies: (0..headers.len()).map(|_| Tally::default()).collect(),
            sample: Vec::new(),
            sample_cap,
            preview,
        })
    }
    pub fn observe(&mut self, row: &Row, source: &str) -> Result<()> {
        self.rows = self.rows.checked_add(1).context("row count overflow")?;
        let slot = sample_slot(self.rows, self.sample_cap);
        let mut sample = slot.map(|_| Sample {
            source: source.to_owned(),
            row: row.number,
            values: vec![None; self.headers.len()],
        });
        for c in &row.cells {
            let t = self
                .tallies
                .get_mut(c.col)
                .context("cell outside table during profiling")?;
            if c.kind == Kind::Missing {
                *t.reasons.entry(c.value.clone()).or_default() += 1;
                continue;
            }
            let tmp;
            let value = if self.inference.types[c.col].kind == Type::Text {
                c.value.as_str()
            } else {
                tmp = match classify(c) {
                    Value::Bool(b) => b.to_string(),
                    Value::Number(d, _) => d.label(),
                    Value::Date(d) => d.label(),
                    Value::Text => bail!("inference mismatch in column {}", c.col + 1),
                };
                &tmp
            };
            t.valid += 1;
            self.sorter.push(c.col as u32, value.as_bytes())?;
            if let Some(s) = &mut sample {
                s.values[c.col] = Some(clipped(value, self.preview));
            }
        }
        if let (Some(i), Some(s)) = (slot, sample) {
            if i == self.sample.len() {
                self.sample.push(s);
            } else {
                self.sample[i] = s;
            }
        }
        Ok(())
    }
    pub fn finish(self) -> Result<Profile> {
        let Self {
            headers,
            inference,
            sorter,
            scratch,
            rows,
            tallies,
            sample,
            ..
        } = self;
        let mut writer = BufWriter::with_capacity(128 << 10, scratch.create()?);
        let mut inputs = headers.into_iter().zip(inference.types).zip(tallies);
        let len = inputs.len();
        let mut pos = 0;
        let mut active: Option<Reduction> = None;
        sorter.finish(|group, key, weight| {
            let group = group as usize;
            while pos < group {
                emit(&mut writer, active.take(), inputs.by_ref(), rows)?;
                pos += 1;
            }
            if active.is_none() {
                let ((name, ty), tally) = inputs.next().context("invalid sort group")?;
                active = Some(Reduction::new(name, ty, tally, rows)?);
            }
            active
                .as_mut()
                .unwrap()
                .add(std::str::from_utf8(key)?, weight)
        })?;
        while pos < len {
            emit(&mut writer, active.take(), inputs.by_ref(), rows)?;
            pos += 1;
        }
        writer.flush()?;
        let file = writer.into_inner().map_err(|e| e.into_error())?;
        Ok(Profile {
            rows,
            columns: Columns { file, len },
            sample,
        })
    }
}
fn emit(
    w: &mut impl Write,
    r: Option<Reduction>,
    i: &mut impl Iterator<Item = ((String, ColumnType), Tally)>,
    rows: u64,
) -> Result<()> {
    check_cancel()?;
    let r = match r {
        Some(r) => r,
        None => {
            let ((n, ty), ta) = i.next().context("missing column")?;
            Reduction::new(n, ty, ta, rows)?
        }
    };
    serde_json::to_writer(&mut *w, &r.finish()?)?;
    w.write_all(b"\n")?;
    Ok(())
}

struct DecimalBins {
    cuts: Vec<Decimal>,
    sum: BigInt,
    power: Option<i32>,
}
impl DecimalBins {
    fn new(lo: &Decimal, hi: &Decimal) -> Self {
        let mut cuts = vec![];
        if lo != hi {
            let p = lo.power.min(hi.power);
            let low = lo.at_power(p);
            let delta = hi.at_power(p) - &low;
            for n in 1..HIST {
                // 1/32 = 3125 × 10^-5. The resulting cut has no rounding error.
                let raw = ((&low * HIST + &delta * n) * 3125usize).to_string();
                cuts.push(Decimal::new(
                    raw.starts_with('-'),
                    raw.trim_start_matches('-'),
                    p - 5,
                ));
            }
        }
        Self {
            cuts,
            sum: BigInt::from(0),
            power: None,
        }
    }
    fn accumulate(&mut self, n: &Decimal, weight: u64) {
        let p = self.power.map_or(n.power, |s| s.min(n.power));
        if let Some(previous) = self.power {
            if previous > p {
                self.sum *= BigInt::from(10u8).pow((previous - p) as u32);
            }
        }
        self.power = Some(p);
        self.sum += n.at_power(p) * weight;
    }
    fn mean(&self, count: u64) -> Option<f64> {
        if count == 0 {
            return None;
        }
        let scaled = (&self.sum * BigInt::from(10u8).pow(40) / count).to_string();
        let label = Decimal::new(
            scaled.starts_with('-'),
            scaled.trim_start_matches('-'),
            self.power? - 40,
        )
        .label();
        label.parse::<f64>().ok().filter(|x| x.is_finite())
    }
}
#[derive(Clone, Copy)]
enum Period {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}
impl Period {
    fn index(self, date: NaiveDate) -> i32 {
        match self {
            Self::Day => date.num_days_from_ce(),
            Self::Week => (date.num_days_from_ce() - 1).div_euclid(7),
            Self::Month => date.year() * 12 + date.month0() as i32,
            Self::Quarter => date.year() * 4 + date.month0() as i32 / 3,
            Self::Year => date.year(),
        }
    }
    fn label(self, i: i32, stride: i32) -> Result<String> {
        Ok(match self {
            Self::Day => NaiveDate::from_num_days_from_ce_opt(i)
                .context("date outside calendar")?
                .to_string(),
            Self::Week => NaiveDate::from_num_days_from_ce_opt(i * 7 + 1)
                .context("week outside calendar")?
                .format("%G-W%V")
                .to_string(),
            Self::Month => format!("{:04}-{:02}", i.div_euclid(12), i.rem_euclid(12) + 1),
            Self::Quarter => format!("{:04}-Q{}", i.div_euclid(4), i.rem_euclid(4) + 1),
            Self::Year if stride == 1 => format!("{i:04}"),
            Self::Year => format!("{i:04} … {:04}", i + stride - 1),
        })
    }
}
struct CalendarBins {
    period: Period,
    first: i32,
    last: i32,
    stride: i32,
}
impl CalendarBins {
    fn new(lo: NaiveDate, hi: NaiveDate) -> Self {
        for period in [
            Period::Day,
            Period::Week,
            Period::Month,
            Period::Quarter,
            Period::Year,
        ] {
            let first = period.index(lo);
            let last = period.index(hi);
            if last - first < TIME_BINS as i32 {
                return Self {
                    period,
                    first,
                    last,
                    stride: 1,
                };
            }
        }
        let first = lo.year();
        let last = hi.year();
        let stride = (last - first + TIME_BINS as i32) / TIME_BINS as i32;
        Self {
            period: Period::Year,
            first,
            last,
            stride,
        }
    }
    fn bin(&self, date: NaiveDate) -> usize {
        ((self.period.index(date) - self.first) / self.stride) as usize
    }
}
struct Reduction {
    col: Column,
    mass: u64,
    number: Option<DecimalBins>,
    days: Option<CalendarBins>,
}
impl Reduction {
    fn new(name: String, ty: ColumnType, t: Tally, rows: u64) -> Result<Self> {
        let missing = rows
            .checked_sub(t.valid)
            .context("multiple values for one row and column")?;
        let blank = missing
            .checked_sub(t.reasons.values().sum())
            .context("duplicate missing value")?;
        let mut reasons = t.reasons;
        if blank > 0 {
            *reasons.entry("absent".into()).or_default() += blank;
        }
        let mut col = Column {
            name,
            kind: ty.kind,
            valid: t.valid,
            missing,
            distinct: 0,
            top: Vec::new(),
            other: 0,
            min: None,
            max: None,
            mean: None,
            bins: vec![],
            missing_reasons: reasons,
        };
        let mut number = None;
        let mut days = None;
        if let Some((lo, hi)) = ty.numbers {
            let hist = DecimalBins::new(&lo, &hi);
            let min = lo.label();
            let max = hi.label();
            if hist.cuts.is_empty() {
                col.bins.push(Bin {
                    label: min.clone(),
                    count: 0,
                });
            } else {
                let mut left = min.clone();
                for (i, right) in hist
                    .cuts
                    .iter()
                    .map(Decimal::label)
                    .chain(std::iter::once(max.clone()))
                    .enumerate()
                {
                    col.bins.push(Bin {
                        label: format!("[{left}, {right}{}", if i + 1 == HIST { "]" } else { ")" }),
                        count: 0,
                    });
                    left = right;
                }
            }
            col.min = Some(min);
            col.max = Some(max);
            number = Some(hist);
        }
        if let Some((lo, hi)) = ty.dates {
            col.min = Some(lo.label());
            col.max = Some(hi.label());
            let calendar = CalendarBins::new(lo.dt.date(), hi.dt.date());
            for i in (calendar.first..=calendar.last).step_by(calendar.stride as usize) {
                col.bins.push(Bin {
                    label: calendar.period.label(i, calendar.stride)?,
                    count: 0,
                });
            }
            days = Some(calendar);
        }
        Ok(Self {
            col,
            mass: 0,
            number,
            days,
        })
    }
    fn add(&mut self, key: &str, weight: u64) -> Result<()> {
        self.mass = self
            .mass
            .checked_add(weight)
            .context("frequency count overflow")?;
        self.col.distinct += 1;
        // Sorter visits complete UTF-8 keys in byte order: prior entries win an equal weight.
        let rank = self.col.top.partition_point(|r| r.count >= weight);
        if rank < TOP {
            if self.col.top.len() == TOP {
                self.col.top.pop();
            }
            self.col.top.insert(
                rank,
                Ranked {
                    value: key.to_owned(),
                    count: weight,
                },
            );
        }
        if let Some(h) = &mut self.number {
            let (v, _) =
                Decimal::parse_with_limit(key, false, 1024).context("invalid canonical number")?;
            let i = h.cuts.partition_point(|b| b <= &v);
            self.col.bins[i].count += weight;
            h.accumulate(&v, weight);
        }
        if let Some(calendar) = &self.days {
            let v = Temporal::parse(key).context("invalid canonical date")?;
            let i = calendar.bin(v.dt.date());
            self.col
                .bins
                .get_mut(i)
                .context("date outside inferred bounds")?
                .count += weight;
        }
        Ok(())
    }
    fn finish(mut self) -> Result<Column> {
        ensure!(
            self.mass == self.col.valid,
            "frequency accounting mismatch in {}",
            self.col.name
        );
        self.col.other = self.col.valid - self.col.top.iter().map(|v| v.count).sum::<u64>();
        if let Some(h) = self.number {
            self.col.mean = h.mean(self.col.valid);
        }
        Ok(self.col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value as Json};
    fn row(number: u64, cells: &[(usize, &str, Kind)]) -> Row {
        Row {
            number,
            width: cells.iter().map(|c| c.0 + 1).max().unwrap_or(0),
            cells: cells
                .iter()
                .map(|(col, value, kind)| Cell {
                    col: *col,
                    value: (*value).into(),
                    kind: *kind,
                })
                .collect(),
            ..Row::default()
        }
    }
    fn report(width: usize, rows: &[Row]) -> Result<Json> {
        let scratch = Scratch::new(None, 64 << 20)?;
        let mut i = Inference::new(width);
        for r in rows {
            i.observe(r)?;
        }
        let headers = (0..width).map(|n| format!("c{n}")).collect::<Vec<_>>();
        let mut p = Profiler::new(&headers, i, &scratch)?;
        for r in rows {
            p.observe(r, "s")?;
        }
        let p = p.finish()?;
        let v = serde_json::to_value(&p)?;
        assert_eq!(serde_json::to_value(&p)?, v);
        Ok(v)
    }
    #[test]
    fn full_column_conflicts_and_sparse_accounting() -> Result<()> {
        let r = report(
            5,
            &[
                row(
                    1,
                    &[
                        (1, "1.0", Kind::Auto),
                        (2, "1.00", Kind::Auto),
                        (3, "empty_cell", Kind::Missing),
                    ],
                ),
                row(2, &[(1, "01", Kind::Auto), (2, "+1e0", Kind::Auto)]),
                row(3, &[(2, "2", Kind::Auto), (3, "hello", Kind::Text)]),
            ],
        )?;
        let cols = r["columns"].as_array().unwrap();
        assert_eq!(cols.len(), 5);
        assert_eq!(cols[0]["kind"], "empty");
        assert_eq!(cols[0]["missing_reasons"], json!({"absent":3}));
        assert_eq!(cols[1]["kind"], "text");
        assert_eq!(
            cols[1]["top"],
            json!([{"value":"01","count":1},{"value":"1.0","count":1}])
        );
        assert_eq!(cols[2]["distinct"], 2);
        assert_eq!(cols[2]["top"][0], json!({"value":"1","count":2}));
        assert_eq!(cols[2]["mean"], json!(4.0 / 3.0));
        assert_eq!(
            cols[3]["missing_reasons"],
            json!({"empty_cell":1,"absent":1})
        );
        assert_eq!(cols[4]["valid"], 0);
        Ok(())
    }
    #[test]
    fn histogram_edges_preserve_sub_float_distances() -> Result<()> {
        let base = 100_000_000_000_000_000u64;
        let rows = (0..=32)
            .map(|i| row(i + 1, &[(0, &(base + i).to_string(), Kind::Number)]))
            .collect::<Vec<_>>();
        let r = report(1, &rows)?;
        let c = &r["columns"][0];
        assert_eq!(c["min"], base.to_string());
        assert_eq!(c["distinct"], 33);
        let bins = c["bins"].as_array().unwrap();
        assert_eq!(bins.len(), 32);
        assert!(bins[..31].iter().all(|b| b["count"] == 1));
        assert_eq!(bins[31]["count"], 2);
        assert_eq!(c["top"].as_array().unwrap().len(), 20);
        assert_eq!(c["other"], 13);
        Ok(())
    }
    #[test]
    fn date_histogram_limits_and_utc_equivalence() -> Result<()> {
        let r = report(
            2,
            &[
                row(
                    1,
                    &[
                        (0, "1900-03-01", Kind::Date),
                        (1, "2025-01-01T01:00:00+01:00", Kind::Auto),
                    ],
                ),
                row(
                    2,
                    &[
                        (0, "2024-02-29", Kind::Date),
                        (1, "2025-01-01T00:00:00Z", Kind::Auto),
                    ],
                ),
            ],
        )?;
        let a = &r["columns"][0];
        assert!(a["bins"].as_array().unwrap().len() <= TIME_BINS);
        assert_eq!(
            a["bins"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["count"].as_u64().unwrap())
                .sum::<u64>(),
            2
        );
        let b = &r["columns"][1];
        assert_eq!(b["kind"], "datetime");
        assert_eq!(b["distinct"], 1);
        assert_eq!(b["min"], "2025-01-01T00:00:00Z");
        Ok(())
    }
    #[test]
    fn calendar_uses_iso_weeks_and_complete_calendar_periods() -> Result<()> {
        let lo = NaiveDate::from_ymd_opt(2020, 12, 31).unwrap();
        for (end, first) in [
            ("2021-01-02", "2020-12-31"),
            ("2022-06-01", "2020-W53"),
            ("2040-01-01", "2020-12"),
            ("2120-01-01", "2020-Q4"),
            ("2400-01-01", "2020"),
        ] {
            let hi = NaiveDate::parse_from_str(end, "%Y-%m-%d")?;
            let bins = CalendarBins::new(lo, hi);
            assert_eq!(bins.period.label(bins.first, bins.stride)?, first);
            assert!(bins.bin(hi) < TIME_BINS);
        }
        let r = report(
            1,
            &[row(1, &[(0, "9999-12-31T23:00:00-02:00", Kind::Date)])],
        )?;
        assert_eq!(r["columns"][0]["bins"][0]["count"], 1);
        Ok(())
    }
    #[test]
    fn inference_roundtrip_merge_remaps_columns() -> Result<()> {
        let mut i = Inference::new(2);
        i.observe(&row(1, &[(0, "3.0", Kind::Auto), (1, "true", Kind::Bool)]))?;
        let i: Inference = serde_json::from_slice(&serde_json::to_vec(&i)?)?;
        let mut g = Inference::new(2);
        g.observe(&row(1, &[(0, "false", Kind::Bool), (1, "2", Kind::Auto)]))?;
        g.merge_mapped(&i, &[1, 0])?;
        assert_eq!(g.types[0].kind, Type::Boolean);
        assert_eq!(g.types[1].kind, Type::Decimal);
        let (lo, hi) = g.types[1].numbers.as_ref().unwrap();
        assert_eq!((lo.label(), hi.label()), ("2".into(), "3".into()));
        Ok(())
    }
    #[test]
    fn complete_long_keys_and_deterministic_bounded_preview() -> Result<()> {
        let x = "ê".repeat(1200);
        let a = format!("{x}a");
        let b = format!("{x}b");
        let rows = (0..150)
            .map(|n| row(n + 1, &[(0, if n % 2 == 0 { &a } else { &b }, Kind::Text)]))
            .collect::<Vec<_>>();
        let r = report(1, &rows)?;
        let c = &r["columns"][0];
        assert_eq!(c["distinct"], 2);
        assert_eq!(c["top"][0]["value"], a);
        assert_eq!(c["top"][1]["value"], b);
        let s = r["sample"].as_array().unwrap();
        assert_eq!(s.len(), 100);
        assert!(s
            .iter()
            .all(|r| r["values"][0].as_str().unwrap().len() <= 128));
        assert_eq!(report(1, &rows)?["sample"], r["sample"]);
        Ok(())
    }
    #[test]
    fn canonical_signs_zeros_and_limits() {
        for v in ["000", "-.00", "+0.0e34"] {
            assert_eq!(Decimal::parse(v, false).unwrap().0.label(), "0");
        }
        assert!(Decimal::parse("001", true).is_none());
        assert!(Decimal::parse("1234567890123456", true).is_none());
        assert!(!supports_number("1e513"));
        assert!(!supports_number("NaN"));
        let vals = [
            "-100", "-10.1", "-1.101", "-1.1", "-0.0001", "0", "0.0001", "1.1", "1.101", "100",
        ];
        let ds = vals.map(|s| Decimal::parse(s, false).unwrap().0);
        assert!(ds.windows(2).all(|w| w[0] < w[1]));
    }
}
