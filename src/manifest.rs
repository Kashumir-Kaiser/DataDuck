//! Ordered, disk-backed source manifests. Only one source is serialized in memory.
use crate::model::Source;
use crate::resource::{Scratch, ScratchFile};
use anyhow::{ensure, Result};
use std::io::{BufRead, BufReader, BufWriter, Write};

pub trait SourceList {
    fn serialize_to<W: Write>(&self, w: W, include_examples: bool) -> Result<()>;
}
fn serialize_one(w: &mut impl Write, s: &Source, examples: bool) -> Result<()> {
    if examples || s.examples.is_empty() {
        serde_json::to_writer(w, s)?;
    } else {
        let mut item = serde_json::to_value(s)?;
        item["examples"] = serde_json::json!([]);
        serde_json::to_writer(w, &item)?;
    }
    Ok(())
}
impl SourceList for [Source] {
    fn serialize_to<W: Write>(&self, mut w: W, include_examples: bool) -> Result<()> {
        w.write_all(b"[")?;
        for (i, s) in self.iter().enumerate() {
            if i > 0 {
                w.write_all(b",")?;
            }
            serialize_one(&mut w, s, include_examples)?;
        }
        w.write_all(b"]")?;
        Ok(())
    }
}
impl SourceList for Vec<Source> {
    fn serialize_to<W: Write>(&self, w: W, e: bool) -> Result<()> {
        self.as_slice().serialize_to(w, e)
    }
}
pub struct Catalog {
    file: BufWriter<ScratchFile>,
    pub rows: u64,
    pub partial: bool,
    count: usize,
}
impl Catalog {
    pub fn new(s: &Scratch) -> Result<Self> {
        Ok(Self {
            file: BufWriter::new(s.create()?),
            rows: 0,
            partial: false,
            count: 0,
        })
    }
    pub fn push(&mut self, s: &Source) -> Result<()> {
        serde_json::to_writer(&mut self.file, s)?;
        self.file.write_all(b"\n")?;
        self.rows += s.rows;
        self.partial |= s.status != "complete";
        self.count += 1;
        Ok(())
    }
    pub fn try_for_each(&self, mut visit: impl FnMut(Source) -> Result<()>) -> Result<()> {
        ensure!(self.file.buffer().is_empty(), "unsealed source manifest");
        let mut reader = BufReader::new(self.file.get_ref().reopen()?);
        let mut line = Vec::new();
        let mut count = 0;
        while reader.read_until(b'\n', &mut line)? != 0 {
            crate::resource::check_cancel()?;
            visit(serde_json::from_slice(&line)?)?;
            count += 1;
            line.clear();
        }
        ensure!(count == self.count, "incomplete source manifest");
        Ok(())
    }
    pub fn seal(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }
}
impl SourceList for Catalog {
    fn serialize_to<W: Write>(&self, mut w: W, include_examples: bool) -> Result<()> {
        ensure!(self.file.buffer().is_empty(), "unsealed source manifest");
        let mut input = BufReader::new(self.file.get_ref().reopen()?);
        let mut line = Vec::new();
        let mut count = 0;
        w.write_all(b"[")?;
        loop {
            line.clear();
            if input.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if count > 0 {
                w.write_all(b",")?;
            }
            count += 1;
            if include_examples {
                w.write_all(&line)?;
            } else {
                let s: Source = serde_json::from_slice(&line)?;
                serialize_one(&mut w, &s, false)?;
            }
        }
        ensure!(count == self.count, "incomplete source manifest");
        w.write_all(b"]")?;
        Ok(())
    }
}
