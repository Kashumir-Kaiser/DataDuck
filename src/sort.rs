//! Exact, byte-bounded external frequency counting.
//!
//! Runs contain (group, key length, weight, full key). Successive runs in a
//! tier are compacted together so neither input size nor cardinality sets the
//! number of active merge readers.

use crate::model::MAX_CELL;
use crate::resource::{check_cancel, Scratch, ScratchFile};
use anyhow::{ensure, Context, Result};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Seek, Write};
use std::mem;

const ARENA_BYTES: usize = 32 * 1024 * 1024;
const INDEX_BYTES: usize = 16 * 1024 * 1024;
const FAN_IN: usize = 16;
const IO_BUFFER_BYTES: usize = 128 * 1024;
const HEADER_BYTES: usize = 16;

#[derive(Clone, Copy)]
struct Entry {
    group: u32,
    start: u32,
    len: u32,
}

impl Entry {
    fn key<'a>(&self, arena: &'a [u8]) -> &'a [u8] {
        let start = self.start as usize;
        &arena[start..start + self.len as usize]
    }
}

fn sort_entries(arena: &[u8], entries: &mut [Entry]) {
    entries.sort_unstable_by(|left, right| {
        left.group
            .cmp(&right.group)
            .then_with(|| left.key(arena).cmp(right.key(arena)))
    });
}

fn emit_entries(
    arena: &[u8],
    entries: &[Entry],
    mut visit: impl FnMut(u32, &[u8], u64) -> Result<()>,
) -> Result<()> {
    let mut pending: Option<(u32, &[u8], u64)> = None;
    for entry in entries {
        let key = entry.key(arena);
        match &mut pending {
            Some((group, prior_key, count)) if entry.group == *group && key == *prior_key => {
                *count = count.checked_add(1).context("frequency count overflow")?;
            }
            _ => {
                if let Some((group, value, count)) = pending {
                    visit(group, value, count)?;
                }
                pending = Some((entry.group, key, 1));
            }
        }
    }
    if let Some((group, value, count)) = pending {
        visit(group, value, count)?;
    }
    Ok(())
}

fn write_record(mut writer: impl Write, group: u32, key: &[u8], count: u64) -> Result<()> {
    let mut header = [0; HEADER_BYTES];
    header[..4].copy_from_slice(&group.to_le_bytes());
    header[4..8].copy_from_slice(&u32::try_from(key.len())?.to_le_bytes());
    header[8..].copy_from_slice(&count.to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(key)?;
    Ok(())
}

// The source is the final tie-breaker, so a heap pop has deterministic order
// even when every input run has the same current key.
struct Head {
    group: u32,
    key: Vec<u8>,
    source: usize,
    count: u64,
}

impl Ord for Head {
    fn cmp(&self, other: &Self) -> Ordering {
        self.group
            .cmp(&other.group)
            .then_with(|| self.key.cmp(&other.key))
            .then_with(|| self.source.cmp(&other.source))
    }
}
impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Head {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Head {}

fn read_record(reader: &mut impl BufRead, head: &mut Head) -> Result<bool> {
    if reader.fill_buf()?.is_empty() {
        return Ok(false);
    }
    let mut header = [0; HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .context("truncated frequency run header")?;
    head.group = u32::from_le_bytes(header[..4].try_into()?);
    let key_len = u32::from_le_bytes(header[4..8].try_into()?) as usize;
    head.count = u64::from_le_bytes(header[8..].try_into()?);
    ensure!(
        key_len <= MAX_CELL && head.count > 0,
        "invalid frequency run record"
    );
    reserve_bounded(&mut head.key, key_len, MAX_CELL)?;
    head.key.resize(key_len, 0);
    reader
        .read_exact(&mut head.key)
        .context("truncated frequency run key")?;
    Ok(true)
}

fn merge<F>(runs: &[ScratchFile], mut visit: F) -> Result<()>
where
    F: FnMut(u32, &[u8], u64) -> Result<()>,
{
    let mut readers = Vec::with_capacity(runs.len());
    let mut heap = BinaryHeap::with_capacity(runs.len());
    for (source, run) in runs.iter().enumerate() {
        let mut input = run.reopen()?;
        input.rewind()?;
        let mut reader = BufReader::with_capacity(IO_BUFFER_BYTES, input);
        let mut head = Head {
            source,
            group: 0,
            key: Vec::new(),
            count: 0,
        };
        if read_record(&mut reader, &mut head)? {
            heap.push(Reverse(head));
        }
        readers.push(reader);
    }
    let mut previous_group = 0;
    let mut previous_key = Vec::new();
    let mut weight = 0u64;
    let mut processed_bytes = 0;
    while let Some(Reverse(mut head)) = heap.pop() {
        processed_bytes += HEADER_BYTES + head.key.len();
        if processed_bytes >= 4 << 20 {
            check_cancel()?;
            processed_bytes = 0;
        }
        if weight == 0 || head.group != previous_group || head.key != previous_key {
            if weight != 0 {
                visit(previous_group, &previous_key, weight)?;
            }
            previous_group = head.group;
            mem::swap(&mut head.key, &mut previous_key);
            weight = head.count;
        } else {
            weight = weight
                .checked_add(head.count)
                .context("frequency count overflow")?;
        }
        if read_record(&mut readers[head.source], &mut head)? {
            heap.push(Reverse(head));
        }
    }
    if weight != 0 {
        visit(previous_group, &previous_key, weight)?;
    }
    Ok(())
}

fn write_run(
    scratch: &Scratch,
    fill: impl FnOnce(&mut BufWriter<&mut ScratchFile>) -> Result<()>,
) -> Result<ScratchFile> {
    let mut run = scratch.create()?;
    {
        let mut writer = BufWriter::with_capacity(IO_BUFFER_BYTES, &mut run);
        fill(&mut writer)?;
        writer.flush()?;
    }
    Ok(run)
}

fn compact(scratch: &Scratch, runs: &[ScratchFile]) -> Result<ScratchFile> {
    write_run(scratch, |writer| {
        merge(runs, |group, key, weight| {
            write_record(&mut *writer, group, key, weight)
        })
    })
}

// Vec::push/extend can otherwise grow capacity past a configured limit by
// doubling. Only grow when necessary, respecting the aggregate allocation cap.
fn reserve_bounded<T>(items: &mut Vec<T>, needed: usize, limit: usize) -> Result<()> {
    if needed > items.capacity() {
        let target = items
            .capacity()
            .saturating_mul(2)
            .max(1024)
            .max(needed)
            .min(limit);
        items.try_reserve_exact(target - items.len())?;
    }
    Ok(())
}

pub(crate) struct Sorter {
    scratch: Scratch,
    arena: Vec<u8>,
    entries: Vec<Entry>,
    tiers: Vec<Vec<ScratchFile>>,
    arena_limit: usize,
    entry_limit: usize,
    fan_in: usize,
}

impl Sorter {
    pub(crate) fn new(scratch: &Scratch) -> Result<Self> {
        Ok(Self {
            scratch: scratch.clone(),
            arena: Vec::new(),
            entries: Vec::new(),
            tiers: Vec::new(),
            arena_limit: ARENA_BYTES,
            entry_limit: INDEX_BYTES / mem::size_of::<Entry>(),
            fan_in: FAN_IN,
        })
    }

    pub(crate) fn push(&mut self, group: u32, key: &[u8]) -> Result<()> {
        ensure!(
            key.len() <= MAX_CELL && key.len() <= self.arena_limit,
            "frequency key exceeds the supported cell size"
        );
        if key.len() > self.arena_limit - self.arena.len() || self.entries.len() == self.entry_limit
        {
            self.flush_entries()?;
        }
        let start = self.arena.len();
        reserve_bounded(&mut self.arena, start + key.len(), self.arena_limit)?;
        let entry_count = self.entries.len() + 1;
        reserve_bounded(&mut self.entries, entry_count, self.entry_limit)?;
        self.arena.extend_from_slice(key);
        self.entries.push(Entry {
            group,
            start: start as u32,
            len: key.len() as u32,
        });
        Ok(())
    }

    fn flush_entries(&mut self) -> Result<()> {
        if self.entries.is_empty() {
            return Ok(());
        }
        sort_entries(&self.arena, &mut self.entries);
        let mut run = write_run(&self.scratch, |writer| {
            emit_entries(&self.arena, &self.entries, |group, key, count| {
                write_record(&mut *writer, group, key, count)
            })
        })?;
        self.arena.clear();
        self.entries.clear();
        let mut level = 0;
        loop {
            if self.tiers.len() == level {
                self.tiers.push(Vec::with_capacity(self.fan_in));
            }
            self.tiers[level].push(run);
            if self.tiers[level].len() < self.fan_in {
                return Ok(());
            }
            run = compact(&self.scratch, &self.tiers[level])?;
            self.tiers[level].clear();
            level += 1;
        }
    }

    pub(crate) fn finish(mut self, visit: impl FnMut(u32, &[u8], u64) -> Result<()>) -> Result<()> {
        check_cancel()?;
        // Small input can be handled without creating any frequency scratch files.
        if self.tiers.is_empty() {
            sort_entries(&self.arena, &mut self.entries);
            return emit_entries(&self.arena, &self.entries, visit);
        }
        self.flush_entries()?;
        drop(self.arena);
        drop(self.entries);
        let mut runs: VecDeque<_> = self.tiers.into_iter().flatten().collect();
        while runs.len() > self.fan_in {
            // Compact the shorter runs first and minimize the number rewritten
            // when only a few excess runs remain.
            let arity = 2 + (runs.len() - 2) % (self.fan_in - 1);
            let inputs: Vec<_> = runs.drain(..arity).collect();
            runs.push_back(compact(&self.scratch, &inputs)?);
        }
        merge(runs.make_contiguous(), visit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Cursor;

    #[test]
    fn merging_many_tiny_runs_is_exact_and_sorted() -> Result<()> {
        let scratch = Scratch::new(None, 16 * 1024 * 1024)?;
        let mut sorter = Sorter::new(&scratch)?;
        sorter.arena_limit = 60;
        sorter.entry_limit = 5;
        sorter.fan_in = 3;
        let mut oracle = BTreeMap::new();
        for i in 0..1000u32 {
            let group = (i * 19) % 11;
            let mut key = format!("key{}", i % 17).into_bytes();
            if i % 5 == 0 {
                key.push(0);
            }
            sorter.push(group, &key)?;
            *oracle.entry((group, key)).or_insert(0u64) += 1;
        }
        sorter.push(3, b"")?;
        oracle.insert((3, Vec::new()), 1);
        let mut got = Vec::new();
        sorter.finish(|group, key, count| {
            got.push(((group, key.to_vec()), count));
            Ok(())
        })?;
        assert_eq!(got, oracle.into_iter().collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn empty_and_unspilled_inputs_use_no_disk_budget() -> Result<()> {
        let scratch = Scratch::new(None, 1)?;
        Sorter::new(&scratch)?.finish(|_, _, _| unreachable!())?;
        let mut sorter = Sorter::new(&scratch)?;
        for _ in 0..8 {
            sorter.push(42, b"x")?;
        }
        let mut count = 0;
        sorter.finish(|g, k, n| {
            assert_eq!((g, k), (42, b"x".as_slice()));
            count += n;
            Ok(())
        })?;
        assert_eq!(count, 8);
        Ok(())
    }

    #[test]
    fn truncated_run_cannot_silently_lose_records() {
        let mut input = Vec::new();
        write_record(&mut input, 42, b"abc", 9).unwrap();
        input.pop();
        let mut head = Head {
            group: 0,
            key: vec![],
            source: 0,
            count: 0,
        };
        assert!(read_record(&mut Cursor::new(input), &mut head).is_err());
    }
}
