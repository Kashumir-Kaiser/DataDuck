//! Coordinates are one-based except cell column offsets, which are zero-based.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_CELL: usize = 1 << 20;
pub const MAX_ROW: usize = 16 << 20;
pub const MAX_COLS: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Auto,
    Text,
    Number,
    Bool,
    Date,
    Missing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cell {
    pub col: usize,
    pub value: String,
    pub kind: Kind,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Row {
    pub number: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub width: usize,
    pub sparse: bool,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Issue {
    pub row: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub col: Option<usize>,
    pub code: String,
    pub message: String,
    pub excerpt: String,
    pub loss: bool,
}

pub enum Event {
    Row(Row),
    Issue(Issue),
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Source {
    pub path: String,
    pub sheet: Option<String>,
    pub status: String,
    pub layout: String,
    pub encoding: String,
    pub rows: u64,
    #[serde(default)]
    pub rejected_rows: u64,
    pub issues: BTreeMap<String, u64>,
    pub examples: Vec<Issue>,
    pub message: String,
    pub table: Option<String>,
    pub rules: serde_json::Value,
}
