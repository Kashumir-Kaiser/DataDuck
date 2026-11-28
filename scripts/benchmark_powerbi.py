#!/usr/bin/env python3
"""Compare four export modes on identical generated inputs. The JSON includes each run and per-mode medians."""
import argparse
import csv
import json
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import tempfile

import benchmark as bench


MODES = ('html', 'html-clean', 'powerbi', 'combined')


def verify_snapshot(project, rows):
    if not (project / f'{project.name}.pbip').is_file():
        raise ValueError('missing PBIP entry point')
    snapshot, = (project / 'DataDuck' / 'snapshots').iterdir()
    fact, = snapshot.glob('fact-*.csv')
    with fact.open(encoding='utf-8-sig', newline='') as stream:
        reader = csv.reader(stream)
        next(reader)
        actual = sum(1 for _ in reader)
    if actual != rows:
        raise ValueError(f'Expected {rows} fact rows; got {actual}')
    with (snapshot / 'profile.csv').open(encoding='utf-8-sig', newline='') as stream:
        fields = 0
        for field in csv.DictReader(stream):
            fields += 1
            if int(field['Valid']) + int(field['Missing']) != rows:
                raise ValueError(f"Invalid profile counts in {field['Field']}")
        if not fields:
            raise ValueError('Empty field profile')
    with (snapshot / 'sources.csv').open(encoding='utf-8-sig', newline='') as stream:
        if sum(int(source['Rows']) for source in csv.DictReader(stream)) != rows:
            raise ValueError('Invalid source audit totals')


def run_workload(exe, mode, inputs, work, case, rows, physical, expanded):
    scratch, html, project = work / 'scratch', work / 'report.html', work / 'Report'
    clean, log = work / 'clean', work / 'cli.log'
    scratch.mkdir()
    pbi = mode in ('combined', 'powerbi')
    if pbi:
        command = [exe, 'powerbi', 'export', inputs, '-o', project]
        if mode == 'combined':
            command.extend(['--html', html])
    else:
        command = [exe, 'build', inputs, '-o', html]
        if mode == 'html-clean':
            command.extend(['--clean-dir', clean])
    result = bench.measured_run(command + ['--temp-dir', scratch], log, scratch)
    errors = []
    if result['exit_code'] != 0:
        errors.append(f"Exit code {result['exit_code']}")
    if mode != 'powerbi':
        errors.append(bench.verify_report(html, case, rows))
    if pbi:
        try:
            verify_snapshot(project, rows)
        except (ValueError, KeyError, StopIteration, OSError, csv.Error) as error:
            errors.append(str(error) or type(error).__name__)
    if mode == 'html-clean' and not list(clean.glob('*.csv')):
        errors.append('Missing cleaned CSV')
    elapsed = max(0.001, result['elapsed_seconds'])
    with log.open(encoding='utf-8', errors='replace') as f:
        captures = sum(line.startswith('Reading ') and line.rstrip().endswith(' bytes)') for line in f)
    if captures != len(list(inputs.iterdir())):
        errors.append('Expected one source-capture event per input file')
    with log.open('rb') as f:
        f.seek(max(0, log.stat().st_size - 4096))
        summary = f.read().decode('utf-8', errors='replace')
    # The log tail may include progress lines from ingestion before the final peak.
    scratch_peaks = [int(n) for n in re.findall(r'peak scratch (\d+) bytes', summary)]
    scratch_peak = max([result['sampled_scratch_bytes'], *scratch_peaks])
    metric = re.search(r'peak working set (\d+) bytes, peak commit (\d+) bytes', summary)
    if metric:
        result['peak_working_set_bytes'] = max(result['peak_working_set_bytes'], int(metric[1]))
        result['peak_commit_bytes'] = max(result['peak_commit_bytes'] or 0, int(metric[2]))
    if result['peak_working_set_bytes'] >= bench.GB or (os.name == 'nt' and not 0 < (result['peak_commit_bytes'] or 0) <= bench.GB):
        errors.append('Process memory exceeds its bound or native commit metric is absent')
    result.update(case=case, mode=mode, rows=rows, physical_bytes=physical, expanded_bytes=expanded,
                  rows_per_second=round(rows / elapsed), input_mib_per_second=round(physical / bench.MIB / elapsed, 2),
                  expanded_mib_per_second=round(expanded / bench.MIB / elapsed, 2), source_capture_events=captures,
                  peak_scratch_bytes=scratch_peak, final_bytes=(html.stat().st_size if html.is_file() else 0)
                  + bench.directory_bytes(project) + bench.directory_bytes(clean), errors=list(filter(None, errors)))
    if result['errors']:
        result['cli_tail'] = summary
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--exe', type=Path, required=True)
    parser.add_argument('--case', choices=bench.CASES, action='append')
    parser.add_argument('--small', action='store_true')
    parser.add_argument('--bytes', type=int, help='Override generator target')
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--baseline-exe', type=Path, help='Reference HTML-only executable for the 5%% comparison')
    parser.add_argument('--work-dir', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error('--repeats must be positive')
    if args.work_dir:
        args.work_dir.mkdir(parents=True, exist_ok=True)
    target = args.bytes if args.bytes is not None else (bench.SMALL_BYTES if args.small else bench.FULL_BYTES)
    if not 1024 <= target <= 2_000_000_000:
        parser.error('--bytes must be between 1024 and 2000000000')
    for executable in (args.exe, args.baseline_exe):
        if executable and not executable.is_file():
            parser.error(f'executable does not exist: {executable}')
    runners = [(mode, args.exe.resolve()) for mode in MODES]
    if args.baseline_exe:
        runners.append(('baseline-html', args.baseline_exe.resolve()))
    results = dict(program=subprocess.check_output([args.exe.resolve(), '--version'], text=True).strip(),
                   platform=platform.platform(), machine=platform.machine(), small=args.small,
                   target_bytes=target, repeats=args.repeats, runs=[], medians=[])
    if args.baseline_exe:
        results['baseline_program'] = subprocess.check_output([args.baseline_exe.resolve(), '--version'], text=True).strip()
    for case in args.case or bench.CASES:
        with tempfile.TemporaryDirectory(prefix='dataduck-pbi-bench-', dir=args.work_dir) as tmp:
            work = Path(tmp)
            inputs = work / 'inputs'
            inputs.mkdir()
            if case in ('xlsx', 'expansion', 'sparse'):
                # Full sparse HTML stresses 16,384 columns; Power BI has a 16,000-object budget.
                rows = bench.generate_xlsx(inputs, case, target, args.small, sparse_columns=8192)
            else:
                rows = bench.generate_csv(inputs, case, target, args.small)
            physical, expanded = bench.input_sizes(inputs)
            timings = {mode: [] for mode, _ in runners}
            for repeat in range(args.repeats):
                # Rotate run order across repeats to avoid assigning cold-cache costs
                # to the same export mode each time.
                offset = repeat % len(runners)
                for mode, exe in runners[offset:] + runners[:offset]:
                    with tempfile.TemporaryDirectory(dir=work) as out:
                        result = run_workload(exe, mode, inputs, Path(out), case, rows, physical, expanded)
                    result.update(mode=mode, repeat=repeat + 1)
                    results['runs'].append(result)
                    timings[mode].append(result['elapsed_seconds'])
                    print(json.dumps(result), flush=True)
            seconds = {mode: statistics.median(values) for mode, values in timings.items()}
            summary = dict(case=case, median_seconds=seconds)
            if 'baseline-html' in seconds:
                ratio = seconds['html'] / max(0.001, seconds['baseline-html'])
                summary.update(html_relative_to_baseline=ratio, investigate_html_slowdown=ratio > 1.05)
            results['medians'].append(summary)
            if args.output:
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(json.dumps(results, indent=2) + '\n', encoding='utf-8')
    return 1 if any(run['errors'] for run in results['runs']) else 0


if __name__ == '__main__':
    raise SystemExit(main())
