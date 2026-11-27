# DataDuck

Turn local Excel workbooks and delimited files into a single interactive HTML dashboard. DataDuck reads the files locally, groups tables with matching columns, and computes exact counts using SSD scratch space. The report embeds its JavaScript, CSS, chart library, and data. You can move the HTML to another computer and open it without the inputs or a network connection.

## Run on Windows

Download `dataduck.exe` from [GitHub Releases](https://github.com/Kashumir-Kaiser/DataDuck/releases), place it in a working directory, and open PowerShell there. A downloaded Google Sheets workbook can be used directly as an XLSX. CSV and TSV exports represent individual sheets.

```powershell
.\dataduck.exe build .\sales.xlsx -o .\sales.html
.\dataduck.exe build .\january.csv .\february.xlsx -o .\report.html
.\dataduck.exe build .\downloads --recursive -o .\report.html --overwrite
.\dataduck.exe inspect .\sales.xlsx --sheet "Transactions" --rows 20
```

Open the resulting HTML in a browser. The table and column selectors show statistics over all accepted rows. Hover or drag across a chart to examine its buckets. The preview searches up to 100 sampled rows per table. Each sampled record includes a source and row number.

For optional exports, choose fresh destinations. Each logical table gets a CSV named in the report. The NDJSON log contains source coordinates, actions, and bounded excerpts. Source data can appear in the dashboard sample, top values, and diagnostics.

```powershell
.\dataduck.exe build .\downloads --config .\recipe.toml -o .\report.html --clean-dir .\cleaned --issues .\issues.ndjson
```

## How data is interpreted

- Matching normalized header names form one logical table. Reordered columns align automatically. An added or removed column creates another table. The first contributing source supplies the display names and order.
- Empty and whitespace-only cells become missing. Text IDs, duplicates, and literal values such as `NA` are preserved. Whole-column inference includes the last accepted row. A text conflict keeps the original lexemes available. Date and numeric distributions use all valid values.
- Numerical, categorical, and date bucket counts, distinct counts, and top-value rankings are exact. The displayed mean is rounded. Time charts count records by date. Missing-value reasons, row repairs, ignored headers, and skipped sources are visible in the source audit.
- A recoverable malformed record is excluded when the reader can identify its end. If CSV framing or worksheet integrity fails, DataDuck discards that entire source. Other valid files or sheets can still produce a partial report. An unreadable workbook-wide structure can affect every sheet in that workbook.
- Visible worksheets are included by default. A sheet-specific recipe can include a hidden worksheet. Excel formulas use cached values. A missing cached value or spreadsheet error becomes a recorded missing cell. DataDuck reads one rectangular table per source or worksheet.
- An explicit header selection takes precedence over Excel table metadata and automatic header detection. For ambiguous headers, inspect the original row coordinates and supply a recipe. An all-text first row inferred as a header is identified in the audit.

## Configure a messy source

Save a UTF-8 TOML file such as `recipe.toml`. Source path globs match input-relative paths. Matching rules must be mutually exclusive. XLSX coordinates are Excel row numbers. Delimited input uses logical records, so a quoted multiline field still belongs to a single record.

```toml
version = 1

[[source]]
path = "monthly-*.xlsx"
sheet = "Transactions"
header_rows = [4]
data_end_row = 80000
skip_repeated_headers = true

[source.columns."Order ID"]
type = "text"

[source.columns."Order Date"]
date_format = "%d/%m/%Y"

[source.columns."Region"]
null_tokens = ["(missing)"]
```

For two header rows, use `header_rows = [4, 5]`. For a headerless source, use `headerless = true` and optionally `names = ["ID", "Amount"]`. Select a one-based inclusive column range with `column_range = [2, 6]`. Set `data_start_row` or `data_end_row` to exclude preambles and footers. Text input supports a delimiter override such as `delimiter = ";"` and an encoding override such as `encoding = "windows-1252"`. UTF-8, BOM-marked UTF-16LE/BE, and explicit BOM-less UTF-16 are supported.

## Resource limits and outcomes

The Windows process installs a **1,000,000,000-byte committed-memory limit** before ingestion. DataDuck streams sparse rows and stores shared strings, committed segments, and sorted frequency runs on disk. The browser has its own memory use. The CLI prints peak scratch and process memory statistics at the end of a successful build.

Scratch defaults to the system temporary directory with a 24 GiB quota and a 2 GiB free-space reserve. A compressed workbook can need several times its physical size. Use an SSD with enough room.

```powershell
.\dataduck.exe build .\downloads -o .\report.html --temp-dir D:\DataDuckTemp --temp-limit-gib 40
```

The default HTML cap is 32 MiB. The writer drops samples and then diagnostic examples if needed, and records those omissions in the report. When the required exact profiles exceed the limit, use the size in the error message to choose `--max-html-mib`. Field, row, and column limits are 1 MiB, 16 MiB, and 16,384 respectively. The portable formats supported are XLSX, CSV, and TSV.

| Exit code | Meaning |
|---|---|
| 0 | Report completed, or inspection succeeded. |
| 2 | Report published with data omissions. Check the source audit. |
| 1 | Processing failed. The previous dashboard is preserved. |
| 64 | Invalid command line or configuration. |

## Build and verify

Use current stable 64-bit Rust. Building requires Cargo and the native toolchain for the target. Running the finished CLI requires only the executable. uPlot 1.6.32 and its MIT license are vendored under `assets/` and included in the report.

```powershell
cargo test --locked
cargo build --locked --release
python scripts/benchmark.py --exe target/release/dataduck.exe --small
```

The benchmark driver generates inputs, measures the release process, and validates report counts against the generated data. Remove `--small` for the full matrix. It covers one large CSV, many files, a large XLSX shared-string table, high compression, wide sparse sheets, and high and low cardinality. The Windows and Linux CI workflow builds the executable and runs the smoke matrix. A manual workflow input runs the full Windows matrix.

Two Linux release runs illustrate the resource tradeoff. A 1.10 GB CSV with one million records used about 40 MiB peak resident memory and 1.15 GB scratch. A 1.098 GB XLSX with 258,672 distinct long labels used about 39 MiB peak resident memory and 3.04 GB scratch. The Windows executable was also exercised under Wine. Native Windows 11 timings and working-set measurements on a 4 GB machine remain unverified.
