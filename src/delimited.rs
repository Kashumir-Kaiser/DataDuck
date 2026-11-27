//! Bounded delimited-text reader. Record and physical line numbers are one-based.
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use anyhow::{bail, Context, Result};
use encoding_rs::WINDOWS_1252;

use crate::model::{Cell, Event, Issue, Kind, Row, MAX_CELL, MAX_COLS, MAX_ROW};

const CHUNK: usize = 64 * 1024;

pub fn read(
    path: &Path,
    delimiter: u8,
    encoding: Option<&str>,
    emit: impl FnMut(Event) -> Result<bool>,
) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    read_stream(file, delimiter, encoding, Limits::default(), emit)
        .with_context(|| format!("reading {}", path.display()))
}

#[derive(Clone, Copy)]
struct Limits {
    cell: usize,
    row: usize,
    cols: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cell: MAX_CELL,
            row: MAX_ROW,
            cols: MAX_COLS,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Windows1252 => "Windows-1252",
        }
    }

    fn detect(prefix: &[u8], explicit: Option<&str>) -> Result<(Self, usize)> {
        let bom = if prefix.starts_with(&[0xef, 0xbb, 0xbf]) {
            Some((Self::Utf8, 3))
        } else if prefix.starts_with(&[0xff, 0xfe]) {
            Some((Self::Utf16Le, 2))
        } else if prefix.starts_with(&[0xfe, 0xff]) {
            Some((Self::Utf16Be, 2))
        } else {
            None
        };
        let Some(name) = explicit else {
            return Ok(bom.unwrap_or((Self::Utf8, 0)));
        };
        let requested = match name.trim().to_ascii_lowercase().as_str() {
            "utf-8" | "utf8" => Self::Utf8,
            "utf-16le" | "utf16le" => Self::Utf16Le,
            "utf-16be" | "utf16be" => Self::Utf16Be,
            "utf-16" | "utf16" => match bom {
                Some((encoding @ (Self::Utf16Le | Self::Utf16Be), _)) => encoding,
                _ => bail!("UTF-16 requires a BOM or an explicit UTF-16LE/UTF-16BE encoding"),
            },
            "windows-1252" | "cp1252" => Self::Windows1252,
            _ => bail!("unknown encoding {name:?}; use UTF-8, UTF-16LE, UTF-16BE or Windows-1252"),
        };
        if let Some((detected, skipped)) = bom {
            if detected != requested {
                bail!(
                    "file BOM indicates {}, recipe requested {}",
                    detected.name(),
                    requested.name()
                );
            }
            Ok((requested, skipped))
        } else {
            Ok((requested, 0))
        }
    }
}

fn read_chunk(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn read_stream(
    mut reader: impl Read,
    delimiter: u8,
    explicit_encoding: Option<&str>,
    limits: Limits,
    emit: impl FnMut(Event) -> Result<bool>,
) -> Result<String> {
    if !delimiter.is_ascii() || matches!(delimiter, b'"' | b'\r' | b'\n' | 0) {
        bail!("delimiter must be a single ASCII character distinct from NUL, quote, CR and LF");
    }

    // Fill the BOM probe even for streams returning one byte per read.
    let mut prefix = [0; 3];
    let mut n = 0;
    while n < prefix.len() {
        let count = read_chunk(&mut reader, &mut prefix[n..])?;
        if count == 0 {
            break;
        }
        n += count;
    }
    let (encoding, skip) = Encoding::detect(&prefix[..n], explicit_encoding)?;
    let mut parser = Parser::new(delimiter, limits, emit);
    let mut decoder = Utf16Decoder::default();
    let mut converted = Vec::new();
    let mut consume = |bytes: &[u8]| -> Result<bool> {
        match encoding {
            Encoding::Utf8 => parser.feed(bytes),
            Encoding::Windows1252 => {
                let (chunk, _) = WINDOWS_1252.decode_without_bom_handling(bytes);
                parser.feed(chunk.as_bytes())
            }
            Encoding::Utf16Le | Encoding::Utf16Be => {
                converted.clear();
                decoder.push(bytes, encoding == Encoding::Utf16Le, &mut converted);
                parser.feed(&converted)
            }
        }
    };
    if !consume(&prefix[skip..n])? {
        return Ok(encoding.name().to_owned());
    }
    let mut chunk = [0u8; CHUNK];
    loop {
        crate::resource::check_cancel()?;
        let size = read_chunk(&mut reader, &mut chunk)?;
        if size == 0 {
            break;
        }
        if !consume(&chunk[..size])? {
            return Ok(encoding.name().to_owned());
        }
    }
    // The sentinel is an invalid UTF-8 byte that cannot be mistaken for CSV syntax.
    if decoder.half.is_some() || decoder.high.is_some() {
        parser.feed(&[0xff])?;
    }
    parser.finish()?;
    Ok(encoding.name().to_owned())
}

#[derive(Default)]
struct Utf16Decoder {
    half: Option<u8>,
    high: Option<u16>,
}

impl Utf16Decoder {
    fn push(&mut self, bytes: &[u8], little_endian: bool, out: &mut Vec<u8>) {
        let to_unit = if little_endian {
            u16::from_le_bytes
        } else {
            u16::from_be_bytes
        };
        let mut remaining = bytes;
        if let Some(first) = self.half {
            let Some((&second, rest)) = remaining.split_first() else {
                return;
            };
            self.half = None;
            self.unit(to_unit([first, second]), out);
            remaining = rest;
        }
        let mut pairs = remaining.chunks_exact(2);
        for pair in &mut pairs {
            self.unit(to_unit([pair[0], pair[1]]), out);
        }
        self.half = pairs.remainder().first().copied();
    }

    fn unit(&mut self, unit: u16, out: &mut Vec<u8>) {
        if let Some(high) = self.high.take() {
            if (0xdc00..=0xdfff).contains(&unit) {
                let n = 0x10000 + (u32::from(high - 0xd800) << 10) + u32::from(unit - 0xdc00);
                out.extend_from_slice(
                    char::from_u32(n)
                        .unwrap()
                        .encode_utf8(&mut [0; 4])
                        .as_bytes(),
                );
                return;
            }
            out.push(0xff);
        }
        match unit {
            0xd800..=0xdbff => self.high = Some(unit),
            0xdc00..=0xdfff => out.push(0xff),
            0..=127 => out.push(unit as u8),
            _ => out.extend_from_slice(
                char::from_u32(u32::from(unit))
                    .unwrap()
                    .encode_utf8(&mut [0; 4])
                    .as_bytes(),
            ),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Start,
    Bare,
    Quoted,
    Closed,
}

#[derive(Clone, Copy)]
enum AfterCr {
    None,
    Boundary,
    Quoted,
}

struct Problem {
    col: usize,
    code: &'static str,
    message: String,
    excerpt: String,
}

struct Parser<F> {
    emit: F,
    delimiter: u8,
    limits: Limits,
    state: State,
    after_cr: AfterCr,
    number: u64,
    line_start: u64,
    line: u64,
    col: usize,
    row_bytes: usize,
    current: Vec<u8>,
    cells: Vec<Cell>,
    problem: Option<Problem>,
}

impl<F: FnMut(Event) -> Result<bool>> Parser<F> {
    fn new(delimiter: u8, limits: Limits, emit: F) -> Self {
        Self {
            emit,
            delimiter,
            limits,
            state: State::Start,
            after_cr: AfterCr::None,
            number: 1,
            line_start: 1,
            line: 1,
            col: 0,
            row_bytes: 0,
            current: Vec::new(),
            cells: Vec::new(),
            problem: None,
        }
    }

    fn reject(&mut self, code: &'static str, message: String) {
        if self.problem.is_none() {
            self.problem = Some(Problem {
                col: self.col,
                code,
                message,
                excerpt: String::from_utf8_lossy(&self.current[..self.current.len().min(128)])
                    .into_owned(),
            });
            self.current.clear();
            self.cells.clear();
        }
    }

    fn charge(&mut self, n: usize) {
        self.row_bytes = self.row_bytes.saturating_add(n);
        if self.row_bytes > self.limits.row && self.problem.is_none() {
            self.reject(
                "row_too_large",
                format!("decoded record exceeds {} bytes", self.limits.row),
            );
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        if self.problem.is_some() {
            return;
        }
        if bytes.len() > self.limits.cell - self.current.len() {
            self.reject(
                "cell_too_large",
                format!("decoded field exceeds {} bytes", self.limits.cell),
            );
        } else {
            self.current.extend_from_slice(bytes);
        }
    }

    fn end_field(&mut self) {
        if self.problem.is_some() {
            return;
        }
        let bytes = std::mem::take(&mut self.current);
        match String::from_utf8(bytes) {
            Ok(value) => self.cells.push(Cell {
                col: self.col,
                value,
                kind: Kind::Auto,
            }),
            Err(error) => {
                self.current = error.into_bytes();
                self.reject(
                    "invalid_encoding",
                    "field contains an invalid encoded sequence".into(),
                );
            }
        }
    }

    fn end_record(&mut self) -> Result<bool> {
        self.end_field();
        let event = if let Some(p) = self.problem.take() {
            Event::Issue(Issue {
                row: self.number,
                line_start: self.line_start,
                line_end: self.line,
                col: Some(p.col),
                code: p.code.into(),
                message: p.message,
                excerpt: p.excerpt,
                loss: true,
            })
        } else {
            Event::Row(Row {
                number: self.number,
                line_start: self.line_start,
                line_end: self.line,
                width: self.col + 1,
                sparse: false,
                cells: std::mem::take(&mut self.cells),
            })
        };
        self.state = State::Start;
        self.col = 0;
        self.row_bytes = 0;
        self.number += 1;
        (self.emit)(event)
    }

    fn feed(&mut self, mut bytes: &[u8]) -> Result<bool> {
        while let Some((&b, rest)) = bytes.split_first() {
            match std::mem::replace(&mut self.after_cr, AfterCr::None) {
                AfterCr::Boundary if b == b'\n' => {
                    bytes = rest;
                    continue;
                }
                AfterCr::Quoted if b == b'\n' => {
                    self.charge(1);
                    self.append(b"\n");
                    bytes = rest;
                    continue;
                }
                _ => {}
            }

            if matches!(b, b'\r' | b'\n') {
                if self.state == State::Quoted {
                    self.charge(1);
                    self.append(&bytes[..1]);
                    if b == b'\r' {
                        self.after_cr = AfterCr::Quoted;
                    }
                } else {
                    if !self.end_record()? {
                        return Ok(false);
                    }
                    self.line_start = self.line + 1;
                    if b == b'\r' {
                        self.after_cr = AfterCr::Boundary;
                    }
                }
                self.line += 1;
                bytes = rest;
                continue;
            }
            if b == self.delimiter && self.state != State::Quoted {
                self.charge(1);
                self.end_field();
                self.col = self.col.saturating_add(1);
                if self.col >= self.limits.cols && self.problem.is_none() {
                    self.reject(
                        "too_many_columns",
                        format!("record exceeds {} columns", self.limits.cols),
                    );
                }
                self.state = State::Start;
                bytes = rest;
                continue;
            }
            if b == b'"' {
                self.charge(1);
                self.state = match self.state {
                    State::Start => State::Quoted,
                    State::Quoted => State::Closed,
                    State::Closed => { self.append(b"\""); State::Quoted }
                    State::Bare => bail!(
                        "uncertain CSV framing: unexpected quote in record {}, physical line {}, column {}",
                        self.number, self.line, self.col + 1
                    ),
                };
                bytes = rest;
                continue;
            }
            if self.state == State::Closed {
                self.reject(
                    "trailing_quoted_data",
                    "unexpected text after a closing quote".into(),
                );
                self.state = State::Bare;
            } else if self.state == State::Start {
                self.state = State::Bare;
            }
            // Append a slice, rather than growing the field a byte at a time. The delimiter
            // is ordinary data inside quoted fields.
            let quoted = self.state == State::Quoted;
            let delimiter = self.delimiter;
            let span = rest
                .iter()
                .position(|&ch| matches!(ch, b'"' | b'\r' | b'\n') || (!quoted && ch == delimiter))
                .map_or(bytes.len(), |index| index + 1);
            self.charge(span);
            self.append(&bytes[..span]);
            bytes = &bytes[span..];
        }
        Ok(true)
    }

    fn finish(&mut self) -> Result<()> {
        if self.state == State::Quoted {
            bail!("uncertain CSV framing: unterminated quoted field in record {}, starting on physical line {}, column {}",
                self.number, self.line_start, self.col + 1);
        }
        if self.row_bytes != 0 {
            self.end_record()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Pieces<'a> {
        bytes: &'a [u8],
        max: usize,
    }
    impl Read for Pieces<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.bytes.len().min(out.len()).min(self.max);
            out[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }

    fn run(
        bytes: &[u8],
        piece: usize,
        encoding: Option<&str>,
        limits: Limits,
    ) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        read_stream(Pieces { bytes, max: piece }, b',', encoding, limits, |e| {
            events.push(e);
            Ok(true)
        })?;
        Ok(events)
    }

    fn row(events: &[Event], i: usize) -> &Row {
        match &events[i] {
            Event::Row(r) => r,
            _ => panic!("expected row"),
        }
    }

    fn issue(events: &[Event], i: usize) -> &Issue {
        match &events[i] {
            Event::Issue(i) => i,
            _ => panic!("expected issue"),
        }
    }

    #[test]
    fn multiline_escaping_coordinates_and_empty_fields_survive_every_split() {
        let input = b"\xef\xbb\xbfa,b,c\r\n,\"left,\r\nright\"\"quoted\",\n,,\r\nx,y,";
        for max in 1..=input.len() {
            let ev = run(input, max, None, Limits::default()).unwrap();
            assert_eq!(ev.len(), 4);
            let r = row(&ev, 1);
            assert_eq!((r.number, r.line_start, r.line_end), (2, 2, 3));
            assert_eq!(r.width, 3);
            assert_eq!(r.cells.len(), 3);
            assert_eq!(r.cells[1].col, 1);
            assert_eq!(r.cells[1].value, "left,\r\nright\"quoted");
            assert_eq!(row(&ev, 2).cells.len(), 3);
            assert!(row(&ev, 2).cells.iter().all(|c| c.value.is_empty()));
            assert_eq!((row(&ev, 3).line_start, row(&ev, 3).width), (5, 3));
        }
    }

    #[test]
    fn detects_utf16_and_carries_surrogates_and_half_units() {
        for little in [true, false] {
            let mut data = if little {
                vec![255, 254]
            } else {
                vec![254, 255]
            };
            for unit in "a,b\n1,\"😃,木\"".encode_utf16() {
                data.extend_from_slice(&if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            let events = run(&data, 1, None, Limits::default()).unwrap();
            assert_eq!(row(&events, 1).cells[1].value, "😃,木");
            let explicit = if little { "utf-16le" } else { "utf-16be" };
            let events = run(&data[2..], 3, Some(explicit), Limits::default()).unwrap();
            assert_eq!(row(&events, 1).cells[1].value, "😃,木");
        }
    }

    #[test]
    fn windows_1252_and_semicolon_are_explicit() {
        let mut events = Vec::new();
        let encoding = read_stream(
            Cursor::new(b"m;v\r\na;\x80"),
            b';',
            Some("cp1252"),
            Limits::default(),
            |e| {
                events.push(e);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(encoding, "Windows-1252");
        assert_eq!(row(&events, 1).cells[1].value, "€");
        assert!(run(b"\xff\xfea\x00", 1, Some("utf8"), Limits::default()).is_err());
    }

    #[test]
    fn bounded_record_rejections_do_not_lose_following_good_data() {
        let limits = Limits {
            cell: 4,
            row: 12,
            cols: 3,
        };
        let data = b"ok,\n12345,\na,b,c,d\naaaa,bbbb,ccc\n1,\xff\n\"a\"b,d\ngood";
        let events = run(data, 2, None, limits).unwrap();
        assert_eq!(events.len(), 7);
        assert_eq!(issue(&events, 1).code, "cell_too_large");
        assert_eq!(issue(&events, 2).code, "too_many_columns");
        assert_eq!(issue(&events, 3).code, "row_too_large");
        assert_eq!(issue(&events, 4).code, "invalid_encoding");
        assert_eq!(issue(&events, 5).code, "trailing_quoted_data");
        assert_eq!(row(&events, 6).cells[0].value, "good");
    }

    #[test]
    fn damaged_utf16_sequence_quarantines_only_the_known_record() {
        let data = [255, 254, b'a', 0, 10, 0, 0, 0xdc, 10, 0, b'z', 0];
        let events = run(&data, 1, None, Limits::default()).unwrap();
        assert_eq!(issue(&events, 1).code, "invalid_encoding");
        assert_eq!(row(&events, 2).cells[0].value, "z");
    }

    #[test]
    fn framing_loss_fails_entire_source_even_while_discarding_oversize_field() {
        for data in [b"a,b\nx,\"unterminated\nmore".as_slice(), b"a,b\nx,y\"z"] {
            let err = run(
                data,
                1,
                None,
                Limits {
                    cell: 2,
                    row: 5,
                    cols: 2,
                },
            )
            .err()
            .expect("source framing should fail");
            assert!(err.to_string().contains("uncertain CSV framing"));
        }
    }

    #[test]
    fn empty_input_blank_line_and_final_quoted_empty_are_distinct() {
        assert!(run(b"", 1, None, Limits::default()).unwrap().is_empty());
        let events = run(b"\r\n\"\"", 1, None, Limits::default()).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(row(&events, 0).cells[0].value, "");
        assert_eq!(row(&events, 1).width, 1);
    }

    #[test]
    fn preview_stops_before_tail_is_parsed() {
        let mut n = 0;
        read_stream(
            Cursor::new(b"a,b\n\"bad tail"),
            b',',
            None,
            Limits::default(),
            |_| {
                n += 1;
                Ok(false)
            },
        )
        .unwrap();
        assert_eq!(n, 1);
    }
}
