#!/usr/bin/env python3
"""Generate reproducible, streaming workloads and measure the actual DataDuck executable.

Examples:
    python scripts/benchmark.py --exe target/release/dataduck --small
    python scripts/benchmark.py --exe target/release/dataduck --case csv --bytes 1100000000
    python scripts/benchmark.py --exe target/release/dataduck.exe --work-dir D:/bench --output results.json

Each case gets fresh inputs, output, and scratch directories; only the JSON results persist.
The full matrix is serial to leave memory and scratch space available to the CLI.
"""

import argparse
import base64
import ctypes
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import subprocess
import sys
import tempfile
import time
from zipfile import ZIP_DEFLATED, ZipFile


CASES = ("csv", "many", "xlsx", "expansion", "sparse", "unique", "repeated")
GB = 1_000_000_000
MIB = 1 << 20
FULL_BYTES = 1_100_000_000
SMALL_BYTES = 512 * 1024
SHEET_NS = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
PKG_NS = "http://schemas.openxmlformats.org/package/2006/relationships"
DOC_NS = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"


def generate_csv(folder, case, target, small):
    count = (8 if small else 128) if case == "many" else 1
    rows = 0
    for part in range(count):
        quota = math.ceil(target / count)
        with (folder / f"part-{part:03d}.csv").open("wb", buffering=MIB) as stream:
            header = b"ID,Region,Amount,Date,Notes\n"
            stream.write(header)
            size = len(header)
            while size < quota:
                index = rows if case != "repeated" else 0
                region = ("North", "South", "", "West")[index % 4]
                notes = f"unique-{index:012x}" if case == "unique" else "weekly-sales"
                row = f"order-{index:012d},{region},{index % 32768}.25,2025-01-{index % 28 + 1:02d},{notes}\n".encode()
                stream.write(row)
                size += len(row)
                rows += 1
    return rows


def xlsx_parts(book, shared):
    part_types = [
        ('/xl/workbook.xml', 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml'),
        ('/xl/worksheets/sheet1.xml', 'application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml'),
    ]
    if shared:
        part_types.append(('/xl/sharedStrings.xml', 'application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml'))
    overrides = ''.join(f'<Override PartName="{name}" ContentType="{mime}"/>' for name, mime in part_types)
    book.writestr('[Content_Types].xml', '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                  '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                  '<Default Extension="xml" ContentType="application/xml"/>' + overrides + '</Types>')
    book.writestr('_rels/.rels', f'<Relationships xmlns="{PKG_NS}"><Relationship Id="r1" Type="{DOC_NS}/officeDocument" Target="xl/workbook.xml"/></Relationships>')
    book.writestr('xl/workbook.xml', f'<workbook xmlns="{SHEET_NS}" xmlns:r="{DOC_NS}"><sheets><sheet name="Data" sheetId="1" r:id="r1"/></sheets></workbook>')
    sst = f'<Relationship Id="r2" Type="{DOC_NS}/sharedStrings" Target="sharedStrings.xml"/>' if shared else ''
    book.writestr('xl/_rels/workbook.xml.rels', f'<Relationships xmlns="{PKG_NS}"><Relationship Id="r1" Type="{DOC_NS}/worksheet" Target="worksheets/sheet1.xml"/>{sst}</Relationships>')


def excel_column(index):
    result = ''
    while index:
        index, remainder = divmod(index - 1, 26)
        result = chr(65 + remainder) + result
    return result


def generate_xlsx(folder, case, target, small):
    with ZipFile(folder / 'book.xlsx', 'w', compression=ZIP_DEFLATED, compresslevel=3, allowZip64=True) as book:
        shared = case == 'xlsx'
        xlsx_parts(book, shared)
        if shared:
            # Incompressible, deterministic text. The final worksheet references every SST key.
            # Stop on physical bytes written, independent of Python/zlib's compression ratio.
            rng = random.Random(4417)
            rows = 0
            with book.open('xl/sharedStrings.xml', 'w', force_zip64=True) as sst:
                sst.write(f'<sst xmlns="{SHEET_NS}">'.encode())
                while rows == 0 or book.fp.tell() < target * 0.995:
                    for _ in range(8):
                        value = base64.b64encode(rng.randbytes(4096))
                        sst.write(b'<si><t>' + value + b'</t></si>')
                    rows += 8
                sst.write(b'</sst>')
        with book.open('xl/worksheets/sheet1.xml', 'w', force_zip64=True) as sheet:
            sheet.write(f'<worksheet xmlns="{SHEET_NS}"><sheetData>'.encode())
            if case == 'sparse':
                columns = 64 if small else 16384
                sheet.write(b'<row r="1">')
                for i in range(1, columns + 1):
                    sheet.write(f'<c r="{excel_column(i)}1" t="inlineStr"><is><t>Field {i}</t></is></c>'.encode())
                sheet.write(b'</row>')
                rows = 64 if small else 4096
                for i in range(1, rows + 1):
                    excel_row = i + 1 if i < rows else 1_048_576
                    col = excel_column(1 + (i * 31) % columns) if i < rows else excel_column(columns)
                    sheet.write(f'<row r="{excel_row}"><c r="{col}{excel_row}"><v>{i}</v></c></row>'.encode())
            else:
                sheet.write(b'<row r="1"><c r="A1" t="inlineStr"><is><t>Key</t></is></c>'
                            b'<c r="B1" t="inlineStr"><is><t>Label</t></is></c></row>')
                if shared:
                    for i in range(rows):
                        r = i + 2
                        sheet.write(f'<row r="{r}"><c r="A{r}"><v>{i}</v></c><c r="B{r}" t="s"><v>{i}</v></c></row>'.encode())
                else:
                    label = b'q' * 4096
                    rows = 0
                    written = 0
                    while written < target:
                        r = rows + 2
                        data = (f'<row r="{r}"><c r="A{r}"><v>{rows % 10}</v></c>'
                                f'<c r="B{r}" t="inlineStr"><is><t>'.encode() + label + b'</t></is></c></row>')
                        sheet.write(data)
                        written += len(data)
                        rows += 1
            sheet.write(b'</sheetData></worksheet>')
    return rows


def input_sizes(folder):
    physical = expanded = 0
    for file in folder.iterdir():
        physical += file.stat().st_size
        if file.suffix == '.xlsx':
            with ZipFile(file) as archive:
                expanded += sum(entry.file_size for entry in archive.infolist())
        else:
            expanded += file.stat().st_size
    return physical, expanded


class WindowsMemory:
    """Read the child's peak counters maintained by the kernel."""
    class Counters(ctypes.Structure):
        _fields_ = [('cb', ctypes.c_uint32), ('page_faults', ctypes.c_uint32)] + [
            (key, ctypes.c_size_t) for key in ('peak_rss', 'rss', 'peak_paged', 'paged', 'peak_nonpaged', 'nonpaged', 'pagefile', 'peak_commit')]

    def __init__(self, pid):
        self.kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        self.kernel.OpenProcess.argtypes = [ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32]
        self.kernel.OpenProcess.restype = ctypes.c_void_p
        self.kernel.CloseHandle.argtypes = [ctypes.c_void_p]
        self.handle = self.kernel.OpenProcess(0x0410, False, pid)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        self.query = ctypes.WinDLL('psapi', use_last_error=True).GetProcessMemoryInfo
        self.query.argtypes = [ctypes.c_void_p, ctypes.POINTER(self.Counters), ctypes.c_uint32]

    def read(self):
        counters = self.Counters()
        counters.cb = ctypes.sizeof(counters)
        if self.query(self.handle, ctypes.byref(counters), counters.cb):
            return counters.peak_rss, counters.peak_commit
        return 0, 0

    def close(self):
        self.kernel.CloseHandle(self.handle)


def directory_bytes(folder):
    size = 0
    for directory, _, files in os.walk(folder):
        for name in files:
            try:
                size += os.stat(os.path.join(directory, name)).st_size
            except FileNotFoundError:
                pass  # A finished merge can unlink runs while the sampler is walking.
    return size


def measured_run(command, logfile, scratch):
    started = time.monotonic()
    peak_rss = peak_scratch = peak_commit = 0
    with logfile.open('w', encoding='utf-8') as log, subprocess.Popen(command, stdout=log, stderr=log) as child:
        monitor = WindowsMemory(child.pid) if os.name == 'nt' else None
        try:
            while True:
                peak_scratch = max(peak_scratch, directory_bytes(scratch))
                if monitor:
                    rss, commit = monitor.read()
                    peak_rss, peak_commit = max(peak_rss, rss), max(peak_commit, commit)
                    if child.poll() is not None:
                        break
                else:
                    pid, status, usage = os.wait4(child.pid, os.WNOHANG)
                    if pid:
                        child.returncode = os.waitstatus_to_exitcode(status)
                        peak_rss = usage.ru_maxrss * (1 if sys.platform == 'darwin' else 1024)
                        break
                time.sleep(0.05)
        finally:
            if monitor:
                monitor.close()
    return dict(exit_code=child.returncode, elapsed_seconds=round(time.monotonic()-started, 3),
                peak_working_set_bytes=peak_rss, peak_commit_bytes=peak_commit or None,
                sampled_scratch_bytes=peak_scratch)


def verify_report(html, case, expected):
    try:
        document = html.read_bytes()
        marker = b'<script type="application/json" id="report-data">'
        start = document.index(marker) + len(marker)
        end = document.index(b'</script>', start)
        data = json.loads(document[start:end])
        assert len(data['tables']) == 1, 'expected one logical table'
        table = data['tables'][0]
        actual = int(table['rows'])
        assert actual == expected, f"expected {expected} accepted rows, report has {actual}"
        assert sum(int(s['rows']) for s in data['sources']) == expected, 'source totals differ'
        if 'profile_id' in table:
            marker = f'<script type="application/json" id="{table["profile_id"]}">'.encode()
            start = document.index(marker) + len(marker)
            end = document.index(b'</script>', start)
            profile = json.loads(document[start:end])
        else:
            profile = table['profile']
        assert int(profile['rows']) == expected, 'profile row count differs'
        columns = {}
        for col in profile['columns']:
            columns[col['name']] = col
            valid = int(col['valid'])
            assert valid + int(col['missing']) == expected, f"{col['name']} valid/missing totals differ"
            assert sum(int(v['count']) for v in col['top']) + int(col['other']) == valid, f"{col['name']} top counts differ"
            if col['bins']:
                assert sum(int(v['count']) for v in col['bins']) == valid, f"{col['name']} bin totals differ"
        if case in ('csv', 'many', 'unique', 'repeated'):
            distinct = 1 if case == 'repeated' else expected
            assert int(columns['ID']['distinct']) == distinct, 'ID distinct count differs'
            missing = 0 if case == 'repeated' else (expected + 1) // 4
            assert int(columns['Region']['missing']) == missing, 'Region null count differs'
        elif case in ('xlsx', 'expansion'):
            assert int(columns['Label']['distinct']) == (expected if case == 'xlsx' else 1), 'label distinct count differs'
        else:
            assert sum(int(col['valid']) for col in columns.values()) == expected, 'sparse cell count differs'
        return None
    except (AssertionError, KeyError, ValueError, OSError) as error:
        return str(error)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--exe', required=True, type=Path)
    parser.add_argument('--case', action='append', choices=CASES, help='Repeat to select cases; default is all.')
    parser.add_argument('--small', action='store_true', help='Use smoke sizes and a narrower sparse workbook.')
    parser.add_argument('--bytes', type=int, help='Override CSV / XLSX physical target and expanded XML target.')
    parser.add_argument('--work-dir', type=Path, help='An SSD location for temporary inputs, scratch and reports.')
    parser.add_argument('--output', type=Path, help='Write machine-readable benchmark results.')
    args = parser.parse_args()
    exe = args.exe.resolve()
    if not exe.is_file():
        parser.error(f'executable does not exist: {exe}')
    target = args.bytes if args.bytes is not None else SMALL_BYTES if args.small else FULL_BYTES
    if not 1024 <= target <= 2_000_000_000:
        parser.error('--bytes must be between 1024 and 2000000000')
    if args.work_dir:
        args.work_dir.mkdir(parents=True, exist_ok=True)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
    version = subprocess.check_output([str(exe), '--version'], text=True).strip()
    results = dict(program=version, platform=platform.platform(), machine=platform.machine(),
                   small=args.small, target_bytes=target, cases=[])
    for case in args.case or CASES:
        print(f'Generating {case} workload...', flush=True)
        with tempfile.TemporaryDirectory(prefix='dataduck-bench-', dir=args.work_dir) as temp:
            work = Path(temp)
            inputs, scratch = work / 'inputs', work / 'scratch'
            inputs.mkdir()
            scratch.mkdir()
            if case in ('xlsx', 'expansion', 'sparse'):
                rows = generate_xlsx(inputs, case, target, args.small)
            else:
                rows = generate_csv(inputs, case, target, args.small)
            physical, expanded = input_sizes(inputs)
            html, log = work / 'report.html', work / 'cli.log'
            cmd = [str(exe), 'build', str(inputs), '-o', str(html), '--temp-dir', str(scratch)]
            print(f'Running {case} ({physical:,} input bytes, {rows:,} expected rows)...', flush=True)
            result = measured_run(cmd, log, scratch)
            result.update(case=case, expected_rows=rows, physical_bytes=physical, expanded_bytes=expanded,
                          rows_per_second=round(rows/max(0.001,result['elapsed_seconds'])),
                          physical_mib_per_second=round(physical/MIB/max(0.001,result['elapsed_seconds']),2),
                          expanded_mib_per_second=round(expanded/MIB/max(0.001,result['elapsed_seconds']),2),
                          report_bytes=html.stat().st_size if html.exists() else None)
            # Preserve only the final CLI message; the JSON stays small for CI artifacts.
            with log.open('rb') as stream:
                stream.seek(max(0, log.stat().st_size-4096))
                result['cli_summary'] = stream.read().decode('utf-8', 'replace').strip()
            result['validation_error'] = verify_report(html, case, rows) if html.exists() else 'missing report'
            scratch_metric = re.search(r'peak scratch (\d+) bytes', result['cli_summary'])
            result['peak_scratch_bytes'] = int(scratch_metric[1]) if scratch_metric else None
            # Tiny processes can exit between Windows polling intervals; use their final native counters.
            metrics = re.search(r'peak working set (\d+) bytes, peak commit (\d+) bytes', result['cli_summary'])
            if metrics:
                result['peak_working_set_bytes'] = max(result['peak_working_set_bytes'], int(metrics[1]))
                result['peak_commit_bytes'] = max(result['peak_commit_bytes'] or 0, int(metrics[2]))
            result['passed'] = (result['validation_error'] is None and result['exit_code']==0 and result['report_bytes'] is not None
                                and result['peak_working_set_bytes'] < GB
                                and (os.name!='nt' or 0 < (result['peak_commit_bytes'] or 0) <= GB))
            results['cases'].append(result)
            print(json.dumps(result, ensure_ascii=True), flush=True)
            if args.output:
                args.output.write_text(json.dumps(results, indent=2) + '\n', encoding='utf-8')
    return 0 if all(r['passed'] for r in results['cases']) else 1


if __name__ == '__main__':
    raise SystemExit(main())
