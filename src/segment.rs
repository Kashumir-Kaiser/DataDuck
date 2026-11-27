//! Internal append-only, sparse row format. All sizes are validated before allocation.
use crate::model::*;
use anyhow::{bail, ensure, Result};
use std::io::{BufRead, Read, Write};

pub const MAGIC: &[u8] = b"DDROW\x01";
#[inline]
fn put(mut n: u64, w: &mut impl Write) -> std::io::Result<()> {
    let mut out = [0u8; 10];
    let mut i = 0;
    while n >= 128 {
        out[i] = n as u8 | 128;
        i += 1;
        n >>= 7;
    }
    out[i] = n as u8;
    w.write_all(&out[..=i])
}
#[inline]
fn get(r: &mut impl Read) -> Result<u64> {
    let mut n = 0u64;
    let mut b = [0u8; 1];
    for shift in (0..=63).step_by(7) {
        r.read_exact(&mut b)?;
        ensure!(shift < 63 || b[0] <= 1, "invalid segment integer");
        n |= u64::from(b[0] & 127) << shift;
        if b[0] < 128 {
            return Ok(n);
        }
    }
    bail!("invalid segment integer")
}
pub fn append(w: &mut impl Write, r: &Row) -> Result<()> {
    put(r.number, w)?;
    put(r.line_start, w)?;
    put(r.line_end - r.line_start, w)?;
    put(((r.width as u64) << 1) | r.sparse as u64, w)?;
    put(r.cells.len() as u64, w)?;
    for c in &r.cells {
        put(((c.col as u64) << 3) | c.kind as u64, w)?;
        put(c.value.len() as u64, w)?;
        w.write_all(c.value.as_bytes())?;
    }
    Ok(())
}
pub fn replay(r: &mut impl BufRead, mut row: impl FnMut(Row) -> Result<()>) -> Result<()> {
    let mut magic = [0; 6];
    r.read_exact(&mut magic)?;
    ensure!(magic == MAGIC, "invalid segment header");
    while !r.fill_buf()?.is_empty() {
        let number = get(r)?;
        let line_start = get(r)?;
        let line_end = line_start
            .checked_add(get(r)?)
            .ok_or_else(|| anyhow::anyhow!("invalid coordinates"))?;
        let flags = get(r)?;
        let width = (flags >> 1) as usize;
        let count = get(r)? as usize;
        ensure!(
            width <= MAX_COLS && count <= width,
            "invalid segment dimensions"
        );
        let mut cells = Vec::with_capacity(count);
        let mut size = 0;
        for _ in 0..count {
            let tag = get(r)?;
            let col = (tag >> 3) as usize;
            let len = usize::try_from(get(r)?)?;
            ensure!(
                col < width && len <= MAX_CELL && len <= MAX_ROW - size,
                "invalid segment cell"
            );
            let kind = match tag & 7 {
                0 => Kind::Auto,
                1 => Kind::Text,
                2 => Kind::Number,
                3 => Kind::Bool,
                4 => Kind::Date,
                5 => Kind::Missing,
                _ => bail!("invalid segment kind"),
            };
            size += len;
            let mut v = vec![0; len];
            r.read_exact(&mut v)?;
            cells.push(Cell {
                col,
                kind,
                value: String::from_utf8(v)?,
            });
        }
        row(Row {
            number,
            line_start,
            line_end,
            width,
            sparse: flags & 1 == 1,
            cells,
        })?;
    }
    Ok(())
}
