//! Streaming SpreadsheetML reader. Shared strings are indexed on disk, and the
//! XML reader is limited *before* it can allocate an oversized event buffer.
use crate::model::{
    Cell, Event as Output, Issue, Kind, Row, MAX_CELL, MAX_COLS, MAX_ROW as ROW_LIMIT,
};
use crate::resource::{Scratch, ScratchFile};
use anyhow::{bail, ensure, Context, Result};
use chrono::{NaiveDate, TimeDelta};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use zip::ZipArchive;

const MAX_ZIP_ENTRIES: u64 = 32_768;
const MAX_DIRECTORY: u64 = 16 << 20;
const MAX_METADATA: u64 = 16 << 20;
const MAX_PART: u64 = 64 << 30;
const MAX_EVENT: usize = 1 << 20;
const MAX_DEPTH: usize = 128;
const MAX_ROW: u32 = 1_048_576;
const MAX_COLUMN: usize = 16_384;

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes(b[..2].try_into().unwrap())
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

// ZipArchive eagerly collects its directory. Check the bounded EOCD trailer
// and ZIP64 record before handing it the file. A workbook is a plain ZIP.
fn open_zip(path: &Path) -> Result<ZipArchive<File>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(65_535 + 22);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity((length - start) as usize);
    Read::by_ref(&mut file)
        .take(65_535 + 22)
        .read_to_end(&mut tail)?;
    let pos = tail
        .windows(4)
        .enumerate()
        .rev()
        .find_map(|(i, sig)| {
            (sig == b"PK\x05\x06"
                && i + 22 <= tail.len()
                && i + 22 + usize::from(le16(&tail[i + 20..])) == tail.len())
            .then_some(i)
        })
        .context("missing ZIP end-of-central-directory record")?;
    let footer = &tail[pos..];
    ensure!(
        le16(&footer[4..]) == 0 && le16(&footer[6..]) == 0,
        "multi-volume ZIP is unsupported"
    );
    let mut count = u64::from(le16(&footer[10..]));
    let mut size = u64::from(le32(&footer[12..]));
    let mut offset = u64::from(le32(&footer[16..]));
    let end_of_central = start + pos as u64;
    let mut directory_end = end_of_central;
    let mut locator = [0; 20];
    let has_zip64 = if end_of_central >= 20 {
        file.seek(SeekFrom::Start(end_of_central - 20))?;
        file.read_exact(&mut locator)?;
        locator.starts_with(b"PK\x06\x07")
    } else {
        false
    };
    if has_zip64 {
        ensure!(
            le32(&locator[4..]) == 0 && le32(&locator[16..]) == 1,
            "multi-volume ZIP64 workbook"
        );
        let record_at = le64(&locator[8..]);
        directory_end = record_at;
        file.seek(SeekFrom::Start(record_at))?;
        let mut record = [0; 56];
        file.read_exact(&mut record)?;
        let record_size = le64(&record[4..]);
        ensure!(
            &record[..4] == b"PK\x06\x06" && (44..=65_536).contains(&record_size),
            "invalid ZIP64 directory footer"
        );
        ensure!(
            record_at.checked_add(12 + record_size) == Some(end_of_central - 20),
            "invalid ZIP64 footer offset"
        );
        ensure!(
            le32(&record[16..]) == 0 && le32(&record[20..]) == 0,
            "multi-volume ZIP64 workbook"
        );
        count = le64(&record[32..]);
        size = le64(&record[40..]);
        offset = le64(&record[48..]);
    }
    ensure!(
        count <= MAX_ZIP_ENTRIES
            && size <= MAX_DIRECTORY
            && offset.checked_add(size).is_some_and(|v| v == directory_end),
        "ZIP directory exceeds supported limits or file bounds"
    );
    // Verify entry lengths within the declared directory before the ZIP library
    // uses their per-record name/extra/comment sizes to allocate metadata.
    file.seek(SeekFrom::Start(offset))?;
    let mut directory = vec![0; size as usize];
    file.read_exact(&mut directory)?;
    let mut entries = directory.as_slice();
    for _ in 0..count {
        ensure!(
            entries.len() >= 46 && entries.starts_with(b"PK\x01\x02"),
            "invalid ZIP central directory entry"
        );
        let length = 46
            + usize::from(le16(&entries[28..]))
            + usize::from(le16(&entries[30..]))
            + usize::from(le16(&entries[32..]));
        entries = entries
            .get(length..)
            .context("truncated ZIP central directory entry")?;
    }
    ensure!(
        entries.is_empty(),
        "unexpected ZIP central directory trailer"
    );
    drop(directory);
    file.seek(SeekFrom::Start(0))?;
    let archive = ZipArchive::new(file)?;
    ensure!(
        archive.len() as u64 == count,
        "ambiguous ZIP directory or duplicate part names"
    );
    Ok(archive)
}

struct EventLimit<R> {
    inner: R,
    left: usize,
    consumed: u64,
}
impl<R: BufRead> BufRead for EventLimit<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.left == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "XML event exceeds byte limit",
            ));
        }
        let buf = self.inner.fill_buf()?;
        Ok(&buf[..buf.len().min(self.left)])
    }
    fn consume(&mut self, n: usize) {
        self.inner.consume(n);
        self.left -= n;
        self.consumed += n as u64;
    }
}
impl<R: BufRead> Read for EventLimit<R> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        let buf = self.fill_buf()?;
        let n = buf.len().min(dst.len());
        dst[..n].copy_from_slice(&buf[..n]);
        self.consume(n);
        Ok(n)
    }
}

fn walk_xml(
    archive: &mut ZipArchive<File>,
    name: &str,
    root: &[u8],
    max_bytes: u64,
    mut visit: impl FnMut(&Event<'_>, usize) -> Result<()>,
) -> Result<()> {
    let part = archive
        .by_name(name)
        .with_context(|| format!("missing XLSX part {name}"))?;
    let advertised = part.size();
    ensure!(
        advertised <= max_bytes,
        "XLSX part {name} exceeds byte limit"
    );
    let bounded = EventLimit {
        inner: BufReader::with_capacity(64 << 10, part.take(max_bytes + 1)),
        left: MAX_EVENT,
        consumed: 0,
    };
    let mut xml = Reader::from_reader(bounded);
    xml.config_mut().expand_empty_elements = true;
    let mut scratch = Vec::with_capacity(4096);
    let mut depth = 0;
    let mut saw_root = false;
    let mut next_cancel = 0;
    loop {
        if xml.buffer_position() >= next_cancel {
            crate::resource::check_cancel()?;
            next_cancel = xml.buffer_position() + (1 << 20);
        }
        xml.get_mut().left = MAX_EVENT;
        let event = xml.read_event_into(&mut scratch).with_context(|| {
            format!(
                "invalid XLSX XML in {name} at byte {}",
                xml.buffer_position()
            )
        })?;
        ensure!(
            xml.get_ref().consumed <= max_bytes,
            "XLSX part {name} exceeds decoded limit"
        );
        match &event {
            Event::Start(start) => {
                if depth == 0 {
                    ensure!(
                        !saw_root && start.local_name().as_ref() == root,
                        "invalid XLSX root in {name}"
                    );
                    saw_root = true;
                }
                depth += 1;
                ensure!(depth <= MAX_DEPTH, "XLSX XML nesting limit in {name}");
                visit(&event, depth)?;
            }
            Event::End(_) => {
                ensure!(depth > 0, "unexpected end in {name}");
                visit(&event, depth)?;
                depth -= 1;
            }
            Event::DocType(_) => bail!("unexpected DOCTYPE in XLSX part {name}"),
            Event::Eof => {
                ensure!(
                    saw_root && depth == 0 && xml.get_ref().consumed == advertised,
                    "incomplete XLSX part {name}"
                );
                break;
            }
            Event::Text(t) if depth == 0 => ensure!(
                t.iter().all(u8::is_ascii_whitespace),
                "unexpected text outside {name} root"
            ),
            _ => {
                visit(&event, depth)?;
            }
        }
        scratch.clear();
    }
    Ok(())
}

fn attr(s: &BytesStart<'_>, local: &[u8]) -> Result<Option<String>> {
    for a in s.attributes() {
        let a = a?;
        if a.key.local_name().as_ref() == local {
            return Ok(Some(a.unescape_value()?.into_owned()));
        }
    }
    Ok(None)
}

fn required(s: &BytesStart<'_>, local: &[u8]) -> Result<String> {
    attr(s, local)?
        .with_context(|| format!("missing XLSX attribute {}", String::from_utf8_lossy(local)))
}

fn relative_part(base: &str, target: &str) -> Result<String> {
    ensure!(
        !target.is_empty() && !target.contains(['\\', '?', '#', ':']),
        "invalid XLSX part target"
    );
    let mut components: Vec<&str> = if target.starts_with('/') {
        vec![]
    } else {
        base.split('/').filter(|c| !c.is_empty()).collect()
    };
    for component in target.split('/') {
        match component {
            "" | "." => (),
            ".." => {
                ensure!(
                    components.pop().is_some(),
                    "XLSX part target escapes package"
                );
            }
            name => components.push(name),
        }
    }
    ensure!(!components.is_empty(), "empty XLSX part path");
    Ok(components.join("/"))
}

#[derive(Clone)]
struct Relation {
    kind: String,
    path: String,
}

fn relationships(
    archive: &mut ZipArchive<File>,
    part: &str,
    base: &str,
) -> Result<HashMap<String, Relation>> {
    let mut rels = HashMap::new();
    walk_xml(
        archive,
        part,
        b"Relationships",
        MAX_METADATA,
        |ev, depth| {
            if let Event::Start(s) = ev {
                if depth == 2
                    && s.local_name().as_ref() == b"Relationship"
                    && attr(s, b"TargetMode")?.as_deref() != Some("External")
                {
                    let kind = required(s, b"Type")?;
                    let suffix = kind.rsplit('/').next().unwrap_or("");
                    if matches!(
                        suffix,
                        "officeDocument"
                            | "worksheet"
                            | "chartsheet"
                            | "dialogsheet"
                            | "xlMacrosheet"
                            | "xlIntlMacrosheet"
                            | "sharedStrings"
                            | "styles"
                            | "table"
                    ) {
                        let key = required(s, b"Id")?;
                        let path = relative_part(base, &required(s, b"Target")?)?;
                        ensure!(
                            rels.insert(
                                key,
                                Relation {
                                    kind: suffix.to_owned(),
                                    path
                                }
                            )
                            .is_none(),
                            "duplicate XLSX relationship"
                        );
                    }
                }
            }
            Ok(())
        },
    )?;
    Ok(rels)
}

#[derive(Clone)]
pub struct Sheet {
    pub name: String,
    path: String,
    missing_rel: Option<String>,
    pub hidden: bool,
}

struct Metadata {
    tabs: Vec<Sheet>,
    styles: Option<String>,
    strings: Option<String>,
    epoch_1904: bool,
}

fn metadata(archive: &mut ZipArchive<File>) -> Result<Metadata> {
    let roots = relationships(archive, "_rels/.rels", "")?;
    let workbook =
        unique_relation(&roots, "officeDocument")?.context("missing XLSX workbook relationship")?;
    let (dir, name) = workbook.rsplit_once('/').unwrap_or(("", &workbook));
    let relation_file = if dir.is_empty() {
        format!("_rels/{name}.rels")
    } else {
        format!("{dir}/_rels/{name}.rels")
    };
    let rels = relationships(archive, &relation_file, dir)?;
    let mut tabs = Vec::new();
    let mut names = std::collections::HashSet::new();
    let mut epoch_1904 = false;
    let mut in_sheets = false;
    walk_xml(archive, &workbook, b"workbook", MAX_METADATA, |e, depth| {
        match e {
            Event::Start(s) if depth == 2 && s.local_name().as_ref() == b"workbookPr" => {
                epoch_1904 = matches!(attr(s, b"date1904")?.as_deref(), Some("true" | "1"));
            }
            Event::Start(s) if depth == 2 && s.local_name().as_ref() == b"sheets" => {
                in_sheets = true
            }
            Event::End(s) if depth == 2 && s.local_name().as_ref() == b"sheets" => {
                in_sheets = false
            }
            Event::Start(s) if depth == 3 && in_sheets && s.local_name().as_ref() == b"sheet" => {
                let rid = required(s, b"id")?;
                let rel = rels.get(&rid);
                if rel.is_some_and(|r| {
                    matches!(
                        r.kind.as_str(),
                        "chartsheet" | "dialogsheet" | "xlMacrosheet" | "xlIntlMacrosheet"
                    )
                }) {
                    return Ok(());
                }
                let name = required(s, b"name")?;
                ensure!(names.insert(name.clone()), "duplicate XLSX worksheet name");
                let hidden = matches!(attr(s, b"state")?.as_deref(), Some("hidden" | "veryHidden"));
                let valid = rel.filter(|r| r.kind == "worksheet");
                tabs.push(Sheet {
                    name,
                    path: valid.map(|r| r.path.clone()).unwrap_or_default(),
                    missing_rel: valid.is_none().then_some(rid),
                    hidden,
                });
            }
            _ => (),
        }
        Ok(())
    })?;
    Ok(Metadata {
        tabs,
        strings: unique_relation(&rels, "sharedStrings")?,
        styles: unique_relation(&rels, "styles")?,
        epoch_1904,
    })
}

fn unique_relation(rels: &HashMap<String, Relation>, kind: &str) -> Result<Option<String>> {
    let mut found = rels.values().filter(|r| r.kind == kind);
    let first = found.next().map(|r| r.path.clone());
    ensure!(
        found.next().is_none(),
        "multiple XLSX relationships for {kind}"
    );
    Ok(first)
}

fn append_text(buf: &mut String, text: &str) -> Result<()> {
    ensure!(
        text.len() <= MAX_CELL.saturating_sub(buf.len()),
        "XLSX cell text exceeds limit"
    );
    buf.push_str(text);
    Ok(())
}

// ECMA-376 ST_Xstring quoting, including the escaped underscore in
// "_x005F_x0041_" and surrogate pairs produced by some exporters.
fn excel_string(raw: &str) -> Result<String> {
    if !raw.contains("_x") {
        return Ok(raw.to_owned());
    }
    let parse = |s: &str| -> Option<u16> {
        let b = s.as_bytes();
        if b.len() < 7
            || &b[..2] != b"_x"
            || b[6] != b'_'
            || !b[2..6].iter().all(u8::is_ascii_hexdigit)
        {
            return None;
        }
        u16::from_str_radix(&s[2..6], 16).ok()
    };
    let mut result = String::with_capacity(raw.len());
    let mut s = raw;
    while let Some(i) = s.find("_x") {
        result.push_str(&s[..i]);
        s = &s[i..];
        if let Some(first) = parse(s) {
            s = &s[7..];
            let n = if (0xd800..=0xdbff).contains(&first) {
                let second = parse(s)
                    .filter(|c| (0xdc00..=0xdfff).contains(c))
                    .context("unpaired Excel Unicode escape")?;
                s = &s[7..];
                0x10000 + ((u32::from(first) - 0xd800) << 10) + u32::from(second) - 0xdc00
            } else {
                u32::from(first)
            };
            result.push(char::from_u32(n).context("invalid Excel Unicode escape")?);
        } else {
            result.push('_');
            s = &s[1..];
        }
    }
    result.push_str(s);
    Ok(result)
}

struct StringTable {
    blob: BufReader<ScratchFile>,
    offsets: BufReader<ScratchFile>,
    count: u64,
    offset_pos: u64,
    blob_pos: u64,
    cache: Vec<Option<(u64, String)>>,
}

impl StringTable {
    fn parse(archive: &mut ZipArchive<File>, part: &str, scratch: &Scratch) -> Result<Self> {
        let mut blob = BufWriter::with_capacity(64 << 10, scratch.create()?);
        let mut offsets = BufWriter::with_capacity(64 << 10, scratch.create()?);
        offsets.write_all(&0u64.to_le_bytes())?;
        let mut end = 0u64;
        let mut count = 0;
        let mut text = String::new();
        let mut in_si = false;
        let mut in_run = false;
        let mut capture = None;
        walk_xml(archive, part, b"sst", MAX_PART, |e, depth| {
            match e {
                Event::Start(s) if depth == 2 && s.local_name().as_ref() == b"si" => {
                    in_si = true;
                    text.clear();
                }
                Event::Start(s) if in_si && depth == 3 && s.local_name().as_ref() == b"r" => {
                    in_run = true
                }
                Event::Start(s)
                    if in_si
                        && (depth == 3 || (in_run && depth == 4))
                        && s.local_name().as_ref() == b"t" =>
                {
                    capture = Some(depth)
                }
                Event::Text(t) if capture == Some(depth) => append_text(&mut text, &t.unescape()?)?,
                Event::CData(t) if capture == Some(depth) => append_text(&mut text, &t.decode()?)?,
                Event::End(s) if capture == Some(depth) && s.local_name().as_ref() == b"t" => {
                    capture = None
                }
                Event::End(s) if in_si && depth == 3 && s.local_name().as_ref() == b"r" => {
                    in_run = false
                }
                Event::End(s) if depth == 2 && s.local_name().as_ref() == b"si" => {
                    let cooked = excel_string(&text)?;
                    blob.write_all(cooked.as_bytes())?;
                    end = end
                        .checked_add(cooked.len() as u64)
                        .context("shared string overflow")?;
                    offsets.write_all(&end.to_le_bytes())?;
                    count += 1;
                    in_si = false;
                }
                _ => (),
            }
            Ok(())
        })?;
        let mut blob = blob.into_inner().map_err(|e| e.into_error())?;
        let mut offsets = offsets.into_inner().map_err(|e| e.into_error())?;
        blob.rewind()?;
        offsets.rewind()?;
        Ok(Self {
            blob: BufReader::new(blob),
            offsets: BufReader::new(offsets),
            count,
            offset_pos: 0,
            blob_pos: 0,
            cache: vec![None; 2048],
        })
    }

    fn get(&mut self, id: u64) -> Result<Option<String>> {
        if id >= self.count {
            return Ok(None);
        }
        let slot = (id % self.cache.len() as u64) as usize;
        if let Some((cached, s)) = &self.cache[slot] {
            if *cached == id {
                return Ok(Some(s.clone()));
            }
        }
        self.offsets
            .seek_relative((id * 8) as i64 - self.offset_pos as i64)?;
        let mut bounds = [0u8; 16];
        self.offsets.read_exact(&mut bounds)?;
        self.offset_pos = id * 8 + 16;
        let start = le64(&bounds[..8]);
        let length = usize::try_from(
            le64(&bounds[8..])
                .checked_sub(start)
                .context("invalid shared string index")?,
        )?;
        ensure!(length <= MAX_CELL, "invalid shared string length");
        self.blob
            .seek_relative(start as i64 - self.blob_pos as i64)?;
        let mut bytes = vec![0; length];
        self.blob.read_exact(&mut bytes)?;
        self.blob_pos = start + length as u64;
        let value = String::from_utf8(bytes)?;
        if length <= 2048 {
            self.cache[slot] = Some((id, value.clone()));
        }
        Ok(Some(value))
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum Format {
    Numeric,
    Date,
    Time,
    DateTime,
    ZeroPad(usize),
}

fn custom_format(code: &str) -> Format {
    if code.len() >= 2 && code.len() <= 64 && code.bytes().all(|c| c == b'0') {
        return Format::ZeroPad(code.len());
    }
    if code.contains(';') {
        return Format::Numeric;
    }
    let mut tokens = String::new();
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                for next in chars.by_ref() {
                    if next == '"' {
                        break;
                    }
                }
            }
            '\\' | '*' | '_' => {
                chars.next();
            }
            '[' => {
                let section: String = chars.by_ref().take_while(|&t| t != ']').collect();
                if !section.starts_with('$')
                    && !matches!(
                        section.to_ascii_lowercase().as_str(),
                        "black"
                            | "blue"
                            | "cyan"
                            | "green"
                            | "magenta"
                            | "red"
                            | "white"
                            | "yellow"
                    )
                {
                    return Format::Numeric;
                }
            }
            c if c.is_ascii_alphabetic() => tokens.push(c.to_ascii_lowercase()),
            _ => (),
        }
    }
    let tokens = tokens.replace("ampm", "").replace("ap", "");
    if tokens.is_empty() || !tokens.bytes().all(|c| b"ymdhis".contains(&c)) {
        return Format::Numeric;
    }
    let date = tokens.contains(['y', 'd']);
    let time = tokens.contains(['h', 's']);
    match (date, time) {
        (true, true) => Format::DateTime,
        (true, false) => Format::Date,
        (false, true) => Format::Time,
        (false, false) if tokens.chars().all(|c| c == 'm') => Format::Date,
        _ => Format::Numeric,
    }
}

fn format_id(id: u32) -> Format {
    match id {
        14..=17 => Format::Date,
        18..=21 | 45 | 47 => Format::Time,
        22 => Format::DateTime,
        _ => Format::Numeric,
    }
}

fn styles(archive: &mut ZipArchive<File>, part: &str) -> Result<Vec<Format>> {
    let mut formats = HashMap::new();
    let mut xfs = vec![];
    let mut in_num = false;
    let mut in_xfs = false;
    walk_xml(archive, part, b"styleSheet", MAX_METADATA, |e, depth| {
        match e {
            Event::Start(s) if depth == 2 => {
                in_num = s.local_name().as_ref() == b"numFmts";
                in_xfs = s.local_name().as_ref() == b"cellXfs";
            }
            Event::Start(s) if in_num && depth == 3 && s.local_name().as_ref() == b"numFmt" => {
                formats.insert(
                    required(s, b"numFmtId")?.parse::<u32>()?,
                    custom_format(&required(s, b"formatCode")?),
                );
                ensure!(formats.len() <= 65_536, "too many number formats");
            }
            Event::Start(s) if in_xfs && depth == 3 && s.local_name().as_ref() == b"xf" => {
                let id = attr(s, b"numFmtId")?
                    .map(|n| n.parse::<u32>())
                    .transpose()?
                    .unwrap_or(0);
                ensure!(xfs.len() < 65_536, "too many cell styles");
                xfs.push(id);
            }
            Event::End(_) if depth == 2 => {
                in_num = false;
                in_xfs = false;
            }
            _ => (),
        }
        Ok(())
    })?;
    Ok(xfs
        .into_iter()
        .map(|id| formats.get(&id).copied().unwrap_or_else(|| format_id(id)))
        .collect())
}

fn serial_date(raw: &str, format: Format, epoch_1904: bool) -> Option<(Kind, String)> {
    let serial = raw.parse::<f64>().ok()?;
    if !serial.is_finite() || !(0.0..2_958_466.0).contains(&serial) {
        return None;
    }
    let day = serial.floor() as i64;
    if !epoch_1904 && day == 60 {
        return None;
    } // Excel's nonexistent 1900-02-29.
    let epoch = if epoch_1904 {
        NaiveDate::from_ymd_opt(1904, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(1899, 12, 31)?
    };
    let days = day - i64::from(!epoch_1904 && day > 60);
    let millis = (serial.fract() * 86_400_000.0).round() as i64;
    let datetime = epoch
        .checked_add_signed(TimeDelta::days(days))?
        .and_hms_opt(0, 0, 0)?
        .checked_add_signed(TimeDelta::milliseconds(millis))?;
    if datetime.date() > NaiveDate::from_ymd_opt(9999, 12, 31)? {
        return None;
    }
    if format == Format::Time && day == 0 {
        return Some((Kind::Text, datetime.time().to_string()));
    }
    if format == Format::Date && millis == 0 {
        Some((Kind::Date, datetime.date().to_string()))
    } else {
        Some((
            Kind::Date,
            format!("{}T{}", datetime.date(), datetime.time()),
        ))
    }
}

fn cell_reference(reference: &str) -> Option<(usize, u32)> {
    let bytes = reference.as_bytes();
    let letters = bytes.iter().take_while(|b| b.is_ascii_alphabetic()).count();
    if letters == 0 || letters > 3 || letters == bytes.len() {
        return None;
    }
    let col = bytes[..letters].iter().fold(0usize, |n, c| {
        n * 26 + usize::from(c.to_ascii_uppercase() - b'A' + 1)
    });
    let digits = &reference[letters..];
    if digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let row: u32 = digits.parse().ok()?;
    (row <= MAX_ROW && col <= MAX_COLUMN).then(|| (col - 1, row))
}

#[derive(Debug)]
struct PreviewDone;
impl std::fmt::Display for PreviewDone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("end of XLSX preview")
    }
}
impl std::error::Error for PreviewDone {}

#[derive(Default)]
struct RawCell {
    col: usize,
    typ: String,
    style: Option<String>,
    has_formula: bool,
    has_value: bool,
    inline: bool,
    raw: String,
}

fn issue(row: u32, col: Option<usize>, code: &str, message: &str, raw: &str) -> Issue {
    Issue {
        row: row.into(),
        line_start: row.into(),
        line_end: row.into(),
        col,
        code: code.to_owned(),
        message: message.to_owned(),
        excerpt: raw.chars().take(120).collect(),
        loss: true,
    }
}

fn cook(
    cell: &RawCell,
    row: u32,
    styles: &[Format],
    strings: &mut Option<StringTable>,
    epoch: bool,
) -> Result<(Cell, Option<Issue>)> {
    let raw = &cell.raw;
    let value = |kind, value| {
        (
            Cell {
                col: cell.col,
                value,
                kind,
            },
            None,
        )
    };
    let bad = |reason: &str, message: &str| {
        (
            Cell {
                col: cell.col,
                value: reason.into(),
                kind: Kind::Missing,
            },
            Some(issue(row, Some(cell.col), reason, message, raw)),
        )
    };
    if cell.has_formula && (!cell.has_value || (raw.trim().is_empty() && cell.typ != "str")) {
        return Ok(bad("uncached_formula", "formula has no cached value"));
    }
    if cell.typ == "inlineStr" && cell.inline {
        return Ok(match excel_string(raw) {
            Ok(s) => value(Kind::Text, s),
            Err(_) => bad("invalid_string", "invalid Excel string escape"),
        });
    }
    if !cell.has_value {
        return Ok(value(Kind::Auto, String::new()));
    }
    Ok(match cell.typ.as_str() {
        "s" => {
            let id = raw.trim().parse::<u64>().ok();
            if let (Some(id), Some(table)) = (id, strings) {
                match table.get(id)? {
                    Some(s) => value(Kind::Text, s),
                    None => bad(
                        "invalid_shared_string",
                        "shared-string index is outside the workbook dictionary",
                    ),
                }
            } else {
                bad(
                    "invalid_shared_string",
                    "invalid shared-string index or missing dictionary",
                )
            }
        }
        "str" => match excel_string(raw) {
            Ok(s) => value(Kind::Text, s),
            Err(_) => bad("invalid_string", "invalid Excel string escape"),
        },
        "d" => {
            let raw = raw.trim();
            if chrono::DateTime::parse_from_rfc3339(raw).is_ok()
                || NaiveDate::parse_from_str(raw, "%Y-%m-%d").is_ok()
                || chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
            {
                value(Kind::Date, raw.to_owned())
            } else {
                bad("invalid_date", "invalid Excel ISO date")
            }
        }
        "e" => bad("excel_error", "Excel error value"),
        "b" => match raw.trim() {
            "0" | "false" => value(Kind::Bool, "false".into()),
            "1" | "true" => value(Kind::Bool, "true".into()),
            _ => bad("invalid_boolean", "invalid Excel boolean value"),
        },
        "" | "n" => {
            let number = raw.trim();
            if number.is_empty() {
                return Ok(value(Kind::Auto, String::new()));
            }
            if !number.parse::<f64>().is_ok_and(f64::is_finite) {
                return Ok(bad("invalid_number", "invalid Excel number"));
            }
            let fmt = match &cell.style {
                Some(s) => match s.parse::<usize>().ok().and_then(|idx| styles.get(idx)) {
                    Some(f) => *f,
                    None if s == "0" && styles.is_empty() => Format::Numeric,
                    None => {
                        // Keep usable source data and surface the unresolvable presentation.
                        let mut warning = issue(
                            row,
                            Some(cell.col),
                            "invalid_style",
                            "unknown cell style; stored number retained",
                            s,
                        );
                        warning.loss = false;
                        return Ok((
                            Cell {
                                col: cell.col,
                                value: number.into(),
                                kind: Kind::Number,
                            },
                            Some(warning),
                        ));
                    }
                },
                None => styles.first().copied().unwrap_or(Format::Numeric),
            };
            match fmt {
                Format::Date | Format::DateTime | Format::Time => {
                    if let Some((kind, s)) = serial_date(number, fmt, epoch) {
                        value(kind, s)
                    } else {
                        bad(
                            "invalid_date",
                            "out-of-range or fictitious Excel serial date",
                        )
                    }
                }
                Format::ZeroPad(width) => {
                    let (whole, frac) = number.split_once('.').unwrap_or((number, ""));
                    if !whole.is_empty()
                        && whole.bytes().all(|b| b.is_ascii_digit())
                        && frac.bytes().all(|b| b == b'0')
                    {
                        let digits = whole.trim_start_matches('0');
                        let digits = if digits.is_empty() { "0" } else { digits };
                        value(Kind::Text, format!("{digits:0>width$}"))
                    } else {
                        value(Kind::Number, number.into())
                    }
                }
                Format::Numeric => value(Kind::Number, number.into()),
            }
        }
        _ => bad("unknown_cell_type", "unsupported Excel cell type"),
    })
}

pub struct Workbook {
    archive: ZipArchive<File>,
    meta: Metadata,
    formats: Vec<Format>,
    strings: Option<StringTable>,
    header_merges: Option<(usize, HeaderRanges)>,
}

impl Workbook {
    pub fn open(path: &Path, scratch: &Scratch) -> Result<Self> {
        let mut archive = open_zip(path)?;
        let meta = metadata(&mut archive)?;
        let formats = meta
            .styles
            .as_ref()
            .map(|p| styles(&mut archive, p))
            .transpose()?
            .unwrap_or_default();
        let strings = meta
            .strings
            .as_ref()
            .map(|p| StringTable::parse(&mut archive, p, scratch))
            .transpose()?;
        Ok(Self {
            archive,
            meta,
            formats,
            strings,
            header_merges: None,
        })
    }
    pub fn sheets(&self) -> &[Sheet] {
        &self.meta.tabs
    }

    pub fn read(
        &mut self,
        index: usize,
        mut emit: impl FnMut(Output) -> Result<bool>,
    ) -> Result<()> {
        let tab = self
            .meta
            .tabs
            .get(index)
            .context("unknown worksheet index")?;
        if let Some(rid) = &tab.missing_rel {
            bail!("missing worksheet relationship {rid} for {}", tab.name);
        }
        let mut send = |event| -> Result<()> {
            if emit(event)? {
                Ok(())
            } else {
                Err(PreviewDone.into())
            }
        };
        let mut in_data = false;
        let mut has_data = false;
        let mut in_row = false;
        let mut row = Row::default();
        let mut last_row = 0u32;
        let mut next_column = 0usize;
        let mut bytes = 0;
        let mut oversized = false;
        let mut cell = None::<RawCell>;
        let mut in_value = false;
        let mut in_inline = false;
        let mut in_run = false;
        let mut text_depth = None;
        let result = walk_xml(
            &mut self.archive,
            &tab.path,
            b"worksheet",
            MAX_PART,
            |ev, depth| {
                match ev {
                    Event::Start(s) if depth == 2 && s.local_name().as_ref() == b"sheetData" => {
                        ensure!(!has_data, "duplicate sheetData");
                        has_data = true;
                        in_data = true;
                    }
                    Event::End(s) if depth == 2 && s.local_name().as_ref() == b"sheetData" => {
                        in_data = false
                    }
                    Event::Start(s)
                        if in_data && depth == 3 && s.local_name().as_ref() == b"row" =>
                    {
                        let n = attr(s, b"r")?
                            .map(|n| n.parse::<u32>())
                            .transpose()?
                            .unwrap_or(last_row + 1);
                        ensure!(
                            n > last_row && n <= MAX_ROW,
                            "out-of-order or invalid XLSX row {n}"
                        );
                        last_row = n;
                        next_column = 0;
                        bytes = 0;
                        oversized = false;
                        in_row = true;
                        row = Row {
                            number: n.into(),
                            line_start: n.into(),
                            line_end: n.into(),
                            sparse: true,
                            ..Row::default()
                        };
                    }
                    Event::Start(s) if in_row && depth == 4 && s.local_name().as_ref() == b"c" => {
                        let col = match attr(s, b"r")? {
                            Some(a) => {
                                let (col, r) =
                                    cell_reference(&a).context("invalid XLSX cell address")?;
                                ensure!(
                                    r == last_row && col >= next_column,
                                    "duplicate, out-of-order or inconsistent XLSX address {a}"
                                );
                                col
                            }
                            None => next_column,
                        };
                        ensure!(col < MAX_COLS, "column exceeds Excel limit");
                        next_column = col + 1;
                        cell = Some(RawCell {
                            col,
                            typ: attr(s, b"t")?.unwrap_or_default(),
                            style: attr(s, b"s")?,
                            ..RawCell::default()
                        });
                    }
                    Event::Start(s) if cell.is_some() && depth == 5 => {
                        let c = cell.as_mut().unwrap();
                        match s.local_name().as_ref() {
                            b"f" => c.has_formula = true,
                            b"v" => {
                                c.has_value = true;
                                in_value = true;
                            }
                            b"is" => {
                                c.inline = true;
                                in_inline = true;
                            }
                            _ => (),
                        }
                    }
                    Event::Start(s)
                        if in_inline && depth == 6 && s.local_name().as_ref() == b"r" =>
                    {
                        in_run = true
                    }
                    Event::Start(s)
                        if in_inline
                            && (depth == 6 || (in_run && depth == 7))
                            && s.local_name().as_ref() == b"t" =>
                    {
                        text_depth = Some(depth)
                    }
                    Event::Text(t)
                        if !oversized
                            && ((in_value && depth == 5) || text_depth == Some(depth)) =>
                    {
                        append_text(&mut cell.as_mut().unwrap().raw, &t.unescape()?)?;
                    }
                    Event::CData(t)
                        if !oversized
                            && ((in_value && depth == 5) || text_depth == Some(depth)) =>
                    {
                        append_text(&mut cell.as_mut().unwrap().raw, &t.decode()?)?;
                    }
                    Event::End(s)
                        if text_depth == Some(depth) && s.local_name().as_ref() == b"t" =>
                    {
                        text_depth = None
                    }
                    Event::End(s) if in_run && depth == 6 && s.local_name().as_ref() == b"r" => {
                        in_run = false
                    }
                    Event::End(s) if depth == 5 && s.local_name().as_ref() == b"is" => {
                        in_inline = false
                    }
                    Event::End(s) if depth == 5 && s.local_name().as_ref() == b"v" => {
                        in_value = false
                    }
                    Event::End(s) if in_row && depth == 4 && s.local_name().as_ref() == b"c" => {
                        let raw = cell.take().context("unexpected end of cell")?;
                        if !oversized {
                            let (value, issue) = cook(
                                &raw,
                                last_row,
                                &self.formats,
                                &mut self.strings,
                                self.meta.epoch_1904,
                            )?;
                            // Formatting-only cells beyond the used data are commonplace.
                            if raw.has_value || raw.has_formula || raw.inline {
                                row.width = value.col + 1;
                            }
                            bytes += value.value.len() + std::mem::size_of::<Cell>();
                            if bytes > ROW_LIMIT {
                                oversized = true;
                                row.cells.clear();
                                send(Output::Issue(self::issue(
                                    last_row,
                                    None,
                                    "oversized_row",
                                    "row exceeded byte limit",
                                    "",
                                )))?;
                            } else {
                                if let Some(issue) = issue {
                                    send(Output::Issue(issue))?;
                                }
                                row.cells.push(value);
                            }
                        }
                    }
                    Event::End(s) if in_row && depth == 3 && s.local_name().as_ref() == b"row" => {
                        if !oversized {
                            if let Some((_, ranges)) =
                                self.header_merges.as_ref().filter(|(i, _)| *i == index)
                            {
                                ranges.apply(&mut row, bytes)?;
                            }
                            send(Output::Row(std::mem::take(&mut row)))?;
                        }
                        in_row = false;
                    }
                    _ => (),
                }
                Ok(())
            },
        );
        if result.as_ref().err().is_some_and(|e| e.is::<PreviewDone>()) {
            return Ok(());
        }
        result.with_context(|| format!("worksheet {}", tab.name))?;
        ensure!(has_data, "missing sheetData in worksheet {}", tab.name);
        Ok(())
    }
}

pub struct Table {
    pub header: u64,
    pub end: u64,
    pub first: usize,
    pub last: usize,
    pub names: Vec<String>,
}

#[derive(Default)]
struct HeaderRanges(HashMap<u64, Vec<(usize, usize)>>);
impl HeaderRanges {
    fn apply(&self, row: &mut Row, mut size: usize) -> Result<()> {
        let Some(ranges) = self.0.get(&row.number) else {
            return Ok(());
        };
        let mut added = Vec::new();
        for &(first, last) in ranges {
            let Ok(i) = row.cells.binary_search_by_key(&first, |c| c.col) else {
                continue;
            };
            let anchor = &row.cells[i];
            if anchor.value.is_empty() || anchor.kind == Kind::Missing {
                continue;
            }
            let mut cursor = i + 1;
            for col in first + 1..=last {
                let existing = row.cells.get(cursor).filter(|c| c.col == col);
                if let Some(existing) = existing {
                    cursor += 1;
                    if !existing.value.is_empty() {
                        continue;
                    }
                }
                size += std::mem::size_of::<Cell>() + anchor.value.len();
                ensure!(
                    size <= ROW_LIMIT,
                    "expanded merged XLSX headers exceed row budget"
                );
                added.push(Cell {
                    col,
                    kind: anchor.kind,
                    value: anchor.value.clone(),
                });
            }
            row.width = row.width.max(last + 1);
        }
        if !added.is_empty() {
            row.cells.retain(|c| {
                !c.value.is_empty() || added.binary_search_by_key(&c.col, |a| a.col).is_err()
            });
            row.cells.extend(added);
            row.cells.sort_unstable_by_key(|c| c.col);
        }
        Ok(())
    }
}

fn range(s: &str) -> Result<(usize, u32, usize, u32)> {
    let (first, last) = s.split_once(':').unwrap_or((s, s));
    let (x, y) = cell_reference(first).context("invalid Excel range")?;
    let (xx, yy) = cell_reference(last).context("invalid Excel range")?;
    ensure!(x <= xx && y <= yy, "reversed Excel range");
    Ok((x, y, xx, yy))
}

impl Workbook {
    /// Workbook tables have a small, separately addressable XML part.
    pub fn table(&mut self, index: usize) -> Result<Option<Table>> {
        let tab = self
            .meta
            .tabs
            .get(index)
            .context("unknown worksheet index")?;
        let (dir, name) = tab.path.rsplit_once('/').unwrap_or(("", tab.path.as_str()));
        let rel_file = if dir.is_empty() {
            format!("_rels/{name}.rels")
        } else {
            format!("{dir}/_rels/{name}.rels")
        };
        if self.archive.index_for_name(&rel_file).is_none() {
            return Ok(None);
        }
        let rels = relationships(&mut self.archive, &rel_file, dir)?;
        let mut table_rels = rels.values().filter(|r| r.kind == "table");
        let Some(rel) = table_rels.next() else {
            return Ok(None);
        };
        if table_rels.next().is_some() {
            return Ok(None);
        }
        let mut result = None;
        let mut in_cols = false;
        walk_xml(
            &mut self.archive,
            &rel.path,
            b"table",
            MAX_METADATA,
            |ev, depth| {
                if let Event::Start(s) = ev {
                    if depth == 1 {
                        let hdr = attr(s, b"headerRowCount")?
                            .map(|s| s.parse::<u32>())
                            .transpose()?
                            .unwrap_or(1);
                        if hdr == 1 {
                            let totals = attr(s, b"totalsRowCount")?
                                .map(|s| s.parse::<u32>())
                                .transpose()?
                                .unwrap_or(0);
                            let (first, start, last, end) = range(&required(s, b"ref")?)?;
                            ensure!(end - start >= totals, "invalid XLSX table bounds");
                            result = Some(Table {
                                header: start.into(),
                                end: (end - totals).into(),
                                first: first + 1,
                                last: last + 1,
                                names: vec![],
                            });
                        }
                    } else if depth == 2 && s.local_name().as_ref() == b"tableColumns" {
                        in_cols = true;
                    } else if in_cols && depth == 3 && s.local_name().as_ref() == b"tableColumn" {
                        if let Some(table) = result.as_mut() {
                            ensure!(table.names.len() < MAX_COLS, "too many XLSX table columns");
                            table.names.push(excel_string(&required(s, b"name")?)?);
                        }
                    }
                } else if let Event::End(s) = ev {
                    if depth == 2 && s.local_name().as_ref() == b"tableColumns" {
                        in_cols = false;
                    }
                }
                Ok(())
            },
        )?;
        if let Some(t) = &result {
            ensure!(
                t.names.len() == t.last - t.first + 1,
                "XLSX table width and names disagree"
            );
        }
        Ok(result)
    }

    /// Resolve horizontal merges solely in explicitly selected header rows.
    /// mergeCells commonly follows sheetData. This extra pass is opt-in.
    pub fn prepare_headers(&mut self, index: usize, rows: &[u64]) -> Result<()> {
        self.header_merges = None;
        if rows.is_empty() {
            return Ok(());
        }
        ensure!(rows.len() <= 32, "at most 32 header rows");
        let tab = self
            .meta
            .tabs
            .get(index)
            .context("unknown worksheet index")?;
        let mut ranges = HeaderRanges::default();
        let mut in_merges = false;
        let mut count = 0;
        walk_xml(
            &mut self.archive,
            &tab.path,
            b"worksheet",
            MAX_PART,
            |ev, depth| {
                match ev {
                    Event::Start(s) if depth == 2 && s.local_name().as_ref() == b"mergeCells" => {
                        in_merges = true
                    }
                    Event::End(s) if depth == 2 && s.local_name().as_ref() == b"mergeCells" => {
                        in_merges = false
                    }
                    Event::Start(s)
                        if in_merges && depth == 3 && s.local_name().as_ref() == b"mergeCell" =>
                    {
                        let (first, top, last, _bottom) = range(&required(s, b"ref")?)?;
                        let top = u64::from(top);
                        if first < last && rows.contains(&top) {
                            count += 1;
                            ensure!(
                                count <= MAX_COLS * rows.len(),
                                "too many merged header ranges"
                            );
                            ranges.0.entry(top).or_default().push((first, last));
                        }
                    }
                    _ => (),
                }
                Ok(())
            },
        )?;
        for spans in ranges.0.values_mut() {
            spans.sort_unstable();
            ensure!(
                spans.windows(2).all(|p| p[0].1 < p[1].0),
                "overlapping merged header ranges"
            );
        }
        self.header_merges = Some((index, ranges));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    const MAIN_SHEET: &str = r#"<worksheet><dimension ref="A1:XFD1048576"/><sheetData>
        <row r="2"><c r="A2" t="s"><v>1</v></c><c r="C2" t="inlineStr"><is><r><t>A&amp;</t></r><rPh><t>phonetic</t></rPh><r><t>B</t></r></is></c></row>
        <row r="4"><c s="1"><v>61.5</v></c><c s="2"><v>7</v></c><c t="str"><f>f</f><v>cached</v></c><c><f>f</f><v/></c><c t="b"><v>0</v></c><c t="e"><v>#N/A</v></c><c t="str"><v>_x005F_x0041_</v></c></row>
        <row r="6"><c r="A6" t="s"><v>1</v></c><c r="B6" s="0"/></row>
        <row r="1048576"><c r="XFD1048576" t="s"><v>0</v></c></row>
        </sheetData><mergeCells><mergeCell ref="A2:B2"/><mergeCell ref="A6:B6"/></mergeCells></worksheet>"#;
    fn fixture(sheet: &str, sst: &str) -> (NamedTempFile, Scratch) {
        let f = NamedTempFile::new().unwrap();
        let mut zip = ZipWriter::new(f.reopen().unwrap());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let entries = [
            (
                "_rels/.rels",
                r#"<Relationships><Relationship Id="p" Type="p/officeDocument" Target="/xl/book.xml"/></Relationships>"#,
            ),
            (
                "xl/book.xml",
                r#"<workbook xmlns:r="r"><sheets><sheet name="Good" r:id="s"/><sheet name="Broken" state="hidden" r:id="b"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/book.xml.rels",
                r#"<Relationships><Relationship Type="r/worksheet" Id="s" Target="sheets/s.xml"/><Relationship Type="r/worksheet" Id="b" Target="sheets/b.xml"/><Relationship Type="r/sharedStrings" Id="ss" Target="ss.xml"/><Relationship Type="r/styles" Id="st" Target="st.xml"/></Relationships>"#,
            ),
            ("xl/ss.xml", sst),
            (
                "xl/st.xml",
                r#"<styleSheet><numFmts><numFmt numFmtId="164" formatCode="0000"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="22"/><xf numFmtId="164"/></cellXfs></styleSheet>"#,
            ),
            ("xl/sheets/s.xml", sheet),
            ("xl/sheets/b.xml", "<worksheet><sheetData><row></sheetData>"),
            (
                "xl/sheets/_rels/s.xml.rels",
                r#"<Relationships><Relationship Type="r/table" Id="tbl" Target="../tables/table.xml"/></Relationships>"#,
            ),
            (
                "xl/tables/table.xml",
                r#"<table ref="A2:C6" totalsRowCount="1"><tableColumns><tableColumn name="Group"/><tableColumn name="Second"/><tableColumn name="Third"/></tableColumns></table>"#,
            ),
        ];
        for (name, s) in entries {
            zip.start_file(name, options).unwrap();
            zip.write_all(s.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        (f, Scratch::new(None, 64 << 20).unwrap())
    }
    const STRINGS: &str = r#"<sst><si><t>final</t></si><si><r><t xml:space="preserve">  Group</t></r><rPh><t>sound</t></rPh><r><t>  </t></r></si></sst>"#;

    #[test]
    fn sparse_types_and_disk_strings() {
        let (f, s) = fixture(&format!("\u{feff}{MAIN_SHEET}"), STRINGS);
        let mut book = Workbook::open(f.path(), &s).unwrap();
        assert_eq!(book.sheets()[0].name, "Good");
        assert!(book.sheets()[1].hidden);
        let mut rows = vec![];
        let mut issues = vec![];
        book.read(0, |e| {
            match e {
                Output::Row(r) => rows.push(r),
                Output::Issue(i) => issues.push((i.row, i.col, i.code)),
            }
            Ok(true)
        })
        .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.number).collect::<Vec<_>>(),
            [2, 4, 6, 1_048_576]
        );
        assert_eq!(
            rows[0]
                .cells
                .iter()
                .map(|c| (c.col, c.value.as_str()))
                .collect::<Vec<_>>(),
            [(0, "  Group  "), (2, "A&B")]
        );
        assert_eq!(
            rows[1]
                .cells
                .iter()
                .map(|c| (c.kind, c.value.as_str()))
                .collect::<Vec<_>>(),
            [
                (Kind::Date, "1900-03-01T12:00:00"),
                (Kind::Text, "0007"),
                (Kind::Text, "cached"),
                (Kind::Missing, "uncached_formula"),
                (Kind::Bool, "false"),
                (Kind::Missing, "excel_error"),
                (Kind::Text, "_x0041_")
            ]
        );
        assert_eq!(issues.len(), 2);
        assert_eq!(rows[2].width, 1);
        assert_eq!(rows[3].width, 16_384);
        assert_eq!(rows[3].cells.len(), 1);
        assert_eq!(rows[3].cells[0].value, "final");
        assert!(s.peak() > 0);
        assert!(book.read(1, |_| Ok(true)).is_err());
        assert!(book.read(0, |_| Ok(false)).is_ok());
    }

    #[test]
    fn table_bounds_and_only_explicit_merged_headers() {
        let (f, s) = fixture(MAIN_SHEET, STRINGS);
        let mut book = Workbook::open(f.path(), &s).unwrap();
        let table = book.table(0).unwrap().unwrap();
        assert_eq!(
            (table.header, table.end, table.first, table.last),
            (2, 5, 1, 3)
        );
        assert_eq!(table.names, ["Group", "Second", "Third"]);
        book.prepare_headers(0, &[2]).unwrap();
        let mut rows = vec![];
        book.read(0, |e| {
            if let Output::Row(r) = e {
                rows.push(r)
            }
            Ok(true)
        })
        .unwrap();
        assert_eq!(rows[0].cells[1].value, "  Group  ");
        assert_eq!(rows[0].cells[1].col, 1);
        assert!(rows[2].cells[1].value.is_empty());
    }

    #[test]
    fn dates_and_addresses_are_checked() {
        assert!(serial_date("60.1", Format::Date, false).is_none());
        assert_eq!(
            serial_date("0", Format::Date, true).unwrap().1,
            "1904-01-01"
        );
        assert_eq!(
            serial_date("59", Format::Date, false).unwrap().1,
            "1900-02-28"
        );
        assert_eq!(
            serial_date("0.25", Format::Time, false).unwrap().1,
            "06:00:00"
        );
        assert_eq!(cell_reference("XFD1048576"), Some((16_383, 1_048_576)));
        assert!(cell_reference("XFE1").is_none());
        assert!(cell_reference("A0").is_none());
    }

    #[test]
    fn limits_apply_to_xml_and_scratch_before_unbounded_growth() {
        let sst = format!("<sst><si><t>{}</t></si></sst>", "a".repeat(MAX_EVENT + 16));
        let (f, s) = fixture(MAIN_SHEET, &sst);
        let err = Workbook::open(f.path(), &s).err().unwrap();
        assert!(format!("{err:#}").contains("XML event exceeds"));
        let (f, _) = fixture(MAIN_SHEET, STRINGS);
        let tiny = Scratch::new(None, 12).unwrap();
        assert!(Workbook::open(f.path(), &tiny).is_err());
        assert!(tiny.check().is_err());
    }
    #[test]
    fn bad_values_preserve_the_rest_of_the_row() {
        let sheet = r#"<worksheet><sheetData><row r="8"><c t="s"><v>9999999</v></c><c t="b"><v>K</v></c><c s="99"><v>5</v></c><c t="d"><v>2026-01-03</v></c><c t="inlineStr"><is><t>_xD83D__xDE03_</t></is></c></row></sheetData></worksheet>"#;
        let (f, s) = fixture(sheet, STRINGS);
        let mut book = Workbook::open(f.path(), &s).unwrap();
        let mut issues = Vec::new();
        let mut row = Row::default();
        book.read(0, |e| {
            match e {
                Output::Issue(i) => issues.push(i),
                Output::Row(r) => row = r,
            }
            Ok(true)
        })
        .unwrap();
        assert_eq!(
            issues
                .iter()
                .map(|i| (i.code.as_str(), i.loss))
                .collect::<Vec<_>>(),
            [
                ("invalid_shared_string", true),
                ("invalid_boolean", true),
                ("invalid_style", false)
            ]
        );
        assert_eq!((row.number, row.width), (8, 5));
        assert_eq!(row.cells[2].value, "5");
        assert_eq!(row.cells[3].kind, Kind::Date);
        assert_eq!(row.cells[4].value, "😃");
    }

    #[test]
    fn crc_mismatch_and_duplicate_cells_are_structural_errors() {
        let (f, s) = fixture(MAIN_SHEET, STRINGS);
        let mut zip = std::fs::read(f.path()).unwrap();
        let start = zip
            .windows(4)
            .enumerate()
            .find_map(|(i, x)| {
                (x == b"PK\x01\x02"
                    && zip
                        .get(i + 46..)
                        .is_some_and(|s| s.starts_with(b"xl/sheets/s.xml")))
                .then_some(i)
            })
            .unwrap();
        zip[start + 16] ^= 1;
        std::fs::write(f.path(), zip).unwrap();
        let mut book = Workbook::open(f.path(), &s).unwrap();
        let error = book.read(0, |_| Ok(true)).unwrap_err();
        assert!(format!("{error:#}").to_lowercase().contains("checksum"));
        let (f, s) = fixture(
            "<worksheet><sheetData><row><c r=\"A1\"/><c r=\"A1\"/></row></sheetData></worksheet>",
            STRINGS,
        );
        let mut book = Workbook::open(f.path(), &s).unwrap();
        let error = book.read(0, |_| Ok(true)).unwrap_err();
        assert!(format!("{error:#}").contains("duplicate"));
    }
}
