//! Evidence for the Power BI import types and the stable axes stored in an export contract.
use super::*;
use chrono::Timelike;
use std::borrow::Cow;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportType {
    Text,
    Boolean,
    Whole,
    Fixed,
    Double,
    Date,
    DateTime,
}

pub fn export_value(cell: &Cell, kind: Type) -> Result<Cow<'_, str>> {
    if kind == Type::Text {
        return Ok(Cow::Borrowed(&cell.value));
    }
    match classify(cell) {
        Value::Bool(b) => Ok(Cow::Borrowed(if b { "true" } else { "false" })),
        Value::Number(d, _) => Ok(Cow::Owned(d.label())),
        Value::Date(d) => Ok(Cow::Owned(d.label())),
        Value::Text => bail!("inference mismatch in column {}", cell.col + 1),
    }
}

fn canonical(s: &str) -> Result<Decimal> {
    Ok(Decimal::parse_with_limit(s, false, 1024)
        .context("invalid canonical number")?
        .0)
}
pub fn compare_numbers(a: &str, b: &str) -> Result<Ordering> {
    Ok(canonical(a)?.cmp(&canonical(b)?))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ImportEvidence {
    integral: bool,
    whole: bool,
    fixed: bool,
    double: bool,
    calendar: bool,
    zoned: bool,
    naive: bool,
}
impl Default for ImportEvidence {
    fn default() -> Self {
        Self {
            integral: true,
            whole: true,
            fixed: true,
            double: true,
            calendar: true,
            zoned: false,
            naive: false,
        }
    }
}

impl Decimal {
    /// Return the signed mantissa at the requested decimal scale, if it fits exactly.
    fn scaled_i64(&self, decimals: i32) -> Option<i64> {
        let power = self.power + decimals;
        if power < 0 || self.digits.len() as i32 + power > 19 {
            return None;
        }
        let mantissa: u64 = self.digits.parse().ok()?;
        let scaled = mantissa.checked_mul(10u64.checked_pow(power as u32)?)?;
        let magnitude = i64::try_from(scaled).ok()?;
        Some(if self.negative { -magnitude } else { magnitude })
    }
    fn safe_double(&self) -> bool {
        if self.digits.len() > 15 {
            return false;
        }
        // Every normal binary64 has at least 15 decimal round-trip digits.
        // Avoid a per-distinct parse/format/parse in the common exponent range.
        let exponent = self.digits.len() as i32 + self.power - 1;
        match exponent {
            -307..=307 => true,
            -308 | 308 => self.label().parse::<f64>().is_ok_and(|v| v.is_normal()),
            _ => false,
        }
    }

    // Compute a short left-edge label using decimal digits so sub-f64 distances
    // and exponents outside a float's range can still be presented unambiguously.
    fn short_label(&self, precision: usize) -> String {
        let rounded = if self.digits.len() > precision {
            let kept = &self.digits[..precision];
            let mut prefix: u64 = kept.parse().expect("at most twelve decimal digits");
            if self.digits.as_bytes()[precision] >= b'5' {
                prefix += 1;
            }
            Decimal::new(
                self.negative,
                &prefix.to_string(),
                self.power + (self.digits.len() - precision) as i32,
            )
        } else {
            self.clone()
        };
        let plain = rounded.label();
        if plain.len() <= 12 {
            return plain;
        }
        let mut value = if rounded.negative {
            "-".to_owned()
        } else {
            String::new()
        };
        value.push(rounded.digits.as_bytes()[0] as char);
        if rounded.digits.len() > 1 {
            value.push('.');
            value.push_str(&rounded.digits[1..]);
        }
        value.push('e');
        value.push_str(&(rounded.power + rounded.digits.len() as i32 - 1).to_string());
        value
    }
}
impl ImportEvidence {
    pub(super) fn numeric(&mut self, v: &Decimal) {
        self.integral &= v.power >= 0;
        self.whole &= v.scaled_i64(0).is_some_and(|n| n < i64::MAX);
        self.fixed &= v.scaled_i64(4).is_some();
        // Evaluate once per distinct decimal, never once per duplicate row.
        if self.double {
            self.double = v.safe_double();
        }
    }
    pub(super) fn temporal(&mut self, d: &Temporal) {
        self.zoned |= d.form == 2;
        self.naive |= d.form == 1;
        let nanos = d.dt.nanosecond();
        self.calendar &= (1900..=9999).contains(&d.dt.year())
            && nanos < 1_000_000_000
            && (u64::from(nanos) * 300).is_multiple_of(1_000_000_000);
    }
}

impl Column {
    pub fn integral(&self) -> bool {
        matches!(self.kind, Type::Integer | Type::Decimal)
            && self.evidence.as_ref().is_some_and(|e| e.integral)
    }
    pub fn utc(&self) -> bool {
        self.kind == Type::Datetime && self.evidence.as_ref().is_some_and(|e| e.zoned && !e.naive)
    }

    /// Compatible narrower values can reuse an existing import contract (e.g. whole -> fixed).
    pub fn supports_import(&self, target: ImportType) -> Result<bool> {
        let e = self
            .evidence
            .as_ref()
            .context("Power BI import evidence unavailable")?;
        if self.valid == 0 || target == ImportType::Text {
            return Ok(true);
        }
        let numeric = matches!(self.kind, Type::Integer | Type::Decimal);
        Ok(match target {
            ImportType::Text => true,
            ImportType::Boolean => self.kind == Type::Boolean,
            ImportType::Whole => numeric && e.whole,
            ImportType::Fixed => numeric && e.fixed,
            ImportType::Double => numeric && e.double,
            ImportType::Date => self.kind == Type::Date && e.calendar,
            ImportType::DateTime => {
                matches!(self.kind, Type::Date | Type::Datetime)
                    && e.calendar
                    && !(e.naive && e.zoned)
            }
        })
    }
    pub fn import_type(&self) -> Result<ImportType> {
        use ImportType as I;
        let order: &[I] = match self.kind {
            Type::Empty | Type::Text => &[I::Text],
            Type::Boolean => &[I::Boolean],
            Type::Date => &[I::Date, I::Text],
            Type::Datetime => &[I::DateTime, I::Text],
            Type::Integer | Type::Decimal => &[I::Whole, I::Fixed, I::Double, I::Text],
        };
        for &candidate in order {
            if self.supports_import(candidate)? {
                return Ok(candidate);
            }
        }
        bail!("missing Power BI import type for {}", self.name)
    }
    fn extrema(&self) -> Result<(&str, &str)> {
        self.min
            .as_deref()
            .zip(self.max.as_deref())
            .context("field has no extrema")
    }
    pub fn histogram(&self) -> Result<HistogramSpec> {
        let (lo, hi) = self.extrema()?;
        let (low, high) = (canonical(lo)?, canonical(hi)?);
        let cuts = DecimalBins::new(&low, &high).cuts;
        Ok(HistogramSpec { low, high, cuts })
    }
    pub fn calendar(&self) -> Result<CalendarSpec> {
        let (lo, hi) = self.extrema()?;
        let start = Temporal::parse(lo)
            .context("invalid calendar minimum")?
            .dt
            .date();
        let end = Temporal::parse(hi)
            .context("invalid calendar maximum")?
            .dt
            .date();
        Ok(CalendarSpec::new(start, end))
    }
}

/// Keys 1..=N are regular intervals. 0 and N+1 are reserved for later snapshots.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistogramSpec {
    low: Decimal,
    high: Decimal,
    cuts: Vec<Decimal>,
}
impl HistogramSpec {
    pub fn key(&self, value: &str) -> Result<i32> {
        let v = canonical(value)?;
        Ok(if v < self.low {
            0
        } else if v > self.high {
            self.cuts.len() as i32 + 2
        } else {
            self.cuts.partition_point(|edge| edge <= &v) as i32 + 1
        })
    }
    pub fn rows(&self) -> Result<Vec<(i32, String, String)>> {
        ensure!(
            self.cuts.len() < HIST && self.low <= self.high,
            "invalid saved histogram bounds"
        );
        let starts: Vec<_> = std::iter::once(&self.low).chain(&self.cuts).collect();
        let mut axis = None;
        for precision in 3..=12 {
            let labels: Vec<_> = starts.iter().map(|v| v.short_label(precision)).collect();
            if labels.iter().all(|s| s.len() <= 12)
                && labels.iter().collect::<BTreeSet<_>>().len() == labels.len()
            {
                axis = Some(labels);
                break;
            }
        }
        let labels =
            axis.unwrap_or_else(|| (1..=starts.len()).map(|n| format!("Bin {n:02}")).collect());
        let mut rows = vec![(0, "Below range".into(), format!("< {}", self.low.label()))];
        for (i, (left, label)) in starts.iter().zip(labels).enumerate() {
            let last = i + 1 == starts.len();
            let end = self.cuts.get(i).unwrap_or(&self.high);
            rows.push((
                i as i32 + 1,
                label,
                format!(
                    "[{}, {}{}",
                    left.label(),
                    end.label(),
                    if last { ']' } else { ')' }
                ),
            ));
        }
        rows.push((
            starts.len() as i32 + 1,
            "Above range".into(),
            format!("> {}", self.high.label()),
        ));
        Ok(rows)
    }
}

impl CalendarSpec {
    fn aligned(&self, index: i32) -> Result<i32> {
        ensure!(self.stride > 0, "invalid saved calendar stride");
        let step = i64::from(self.stride);
        let first = i64::from(self.first);
        Ok(i32::try_from(
            first + (i64::from(index) - first).div_euclid(step) * step,
        )?)
    }
    /// Keys use absolute calendar indices. Extending a range backwards preserves all old keys.
    pub fn key(&self, value: &str) -> Result<i32> {
        let t = Temporal::parse(value).context("invalid calendar axis value")?;
        self.aligned(self.period.index(t.dt.date()))
    }
    pub fn extend(&mut self, min: &str, max: &str) -> Result<()> {
        let first = self.first.min(self.key(min)?);
        let last = self.last.max(self.key(max)?);
        ensure!(
            (i64::from(last) - i64::from(first)) / i64::from(self.stride) < TIME_BINS as i64,
            "saved calendar grain would exceed {TIME_BINS} periods; export a fresh project"
        );
        self.first = first;
        self.last = last;
        Ok(())
    }
    pub fn rows(&self) -> Result<Vec<(i32, String, String)>> {
        ensure!(
            self.stride > 0
                && self.first <= self.last
                && (i64::from(self.last) - i64::from(self.first)) / i64::from(self.stride)
                    < TIME_BINS as i64,
            "invalid saved calendar range"
        );
        (self.first..=self.last)
            .step_by(self.stride as usize)
            .map(|k| {
                let label = self.period.label(k, self.stride)?;
                Ok((k, label.clone(), label))
            })
            .collect()
    }
}
