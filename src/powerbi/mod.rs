mod facts;
mod pipeline;
mod planner;
mod render;
mod storage;
pub(crate) use pipeline::{run, Command};

use crate::profile::{CalendarSpec, HistogramSpec, ImportType, Type};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Contract {
    pub version: u32,
    pub template_version: u32,
    pub tables: Vec<Table>,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Table {
    pub id: String,
    pub name: String,
    pub fields: Vec<Field>,
    pub axes: Vec<Axis>,
    pub charts: Vec<Chart>,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Field {
    pub name: String,
    pub key: String,
    pub kind: Type,
    pub import: ImportType,
    #[serde(default)]
    pub utc: bool,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Axis {
    pub field: usize,
    pub spec: AxisSpec,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "values", rename_all = "snake_case")]
pub enum AxisSpec {
    Categories(Vec<String>),
    Years,
    Discrete(Vec<String>),
    Histogram(HistogramSpec),
    Calendar(CalendarSpec),
}
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    Average,
    Sum,
    Min,
    Max,
}
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Visual {
    Bars,
    Columns,
    Line,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Chart {
    pub axis: usize, // field ordinal, not index in axes
    pub measure: Option<usize>,
    pub aggregate: Option<Aggregate>,
    pub visual: Visual,
    pub title: String,
    pub rule: String,
}
