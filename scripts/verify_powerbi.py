#!/usr/bin/env python3
"""Generate a synthetic PBIP and check every report/project JSON file against pinned local Microsoft schemas.

Use the schema checkout from .github/workflows/verify.yml and install jsonschema==4.26.0.
The output is retained for the separate Microsoft TOM validation step.
"""
import argparse
import csv
from functools import cache
import json
from pathlib import Path
import subprocess

from jsonschema.validators import validator_for
from referencing import Registry, Resource
from referencing.exceptions import NoSuchResource


SCHEMA_BASE = 'https://developer.microsoft.com/json-schemas/'


def validate_project(project, schema_root):
    @cache
    def retrieve(uri):
        if not uri.startswith(SCHEMA_BASE):
            raise NoSuchResource(ref=uri)
        path = (schema_root / uri.removeprefix(SCHEMA_BASE)).resolve()
        if not path.is_relative_to(schema_root) or not path.is_file():
            raise NoSuchResource(ref=uri)
        return Resource.from_contents(json.loads(path.read_text(encoding='utf-8-sig')))

    registry = Registry(retrieve=retrieve)
    report = next(project.glob('*.Report'))
    model = next(project.glob('*.SemanticModel'))
    entrypoint, = project.glob('*.pbip')
    files = [entrypoint, report / 'definition.pbir', model / 'definition.pbism']
    files.extend(sorted((report / 'definition').rglob('*.json')))
    if not any(file.name == 'visual.json' for file in files):
        raise ValueError('No PBIR visual descriptors found')
    contents = {}
    for file in files:
        data = json.loads(file.read_text(encoding='utf-8-sig'))
        contents[file] = data
        schema = retrieve(data['$schema']).contents
        validator = validator_for(schema)(schema, registry=registry)
        for error in validator.iter_errors(data):
            raise ValueError(f'{file.relative_to(project)} {error.json_path}: {error.message}')
    if (project / contents[entrypoint]['artifacts'][0]['report']['path']).resolve() != report:
        raise ValueError('The PBIP entry point does not open the generated report')
    model_ref = contents[report / 'definition.pbir']['datasetReference']['byPath']['path']
    if (report / model_ref).resolve() != model:
        raise ValueError('The report does not resolve its local semantic model')
    pages = report / 'definition' / 'pages'
    page_meta = contents[pages / 'pages.json']
    page_files = sorted(pages.glob('*/page.json'))
    page_names = [contents[file]['name'] for file in page_files]
    if (sorted(page_meta['pageOrder']) != sorted(page_names)
            or len(set(page_names)) != len(page_names)
            or page_meta['activePageName'] not in page_names):
        raise ValueError('Page order or active page does not resolve')
    for file in page_files:
        page = contents[file]
        if page['name'] != file.parent.name:
            raise ValueError(f'Page folder and name differ in {file}')
        visuals = set()
        for descriptor in file.parent.glob('visuals/*/visual.json'):
            name = contents[descriptor]['name']
            if name in visuals or name != descriptor.parent.name:
                raise ValueError(f'Invalid visual identity in {descriptor}')
            visuals.add(name)
        for link in page.get('visualInteractions', []):
            if link['source'] not in visuals or link['target'] not in visuals:
                raise ValueError(f'Unknown interaction endpoint in {file}')
    for package in contents[report / 'definition' / 'report.json'].get('resourcePackages', []):
        for item in package['items']:
            path = report / 'StaticResources' / package['name'] / item['path']
            if not path.resolve().is_relative_to(report) or not path.is_file():
                raise ValueError(f'Missing report resource {item["path"]}')
    print(f'Validated {len(files)} project descriptors against local Microsoft schemas.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--project', type=Path, required=True)
    parser.add_argument('--schemas', type=Path, required=True)
    parser.add_argument('--exe', type=Path, help='Generate a new synthetic project before validating.')
    args = parser.parse_args()
    project = args.project.resolve()
    if args.exe:
        project.parent.mkdir(parents=True, exist_ok=True)
        source = project.parent / 'synthetic.csv'
        with source.open('x', encoding='utf-8', newline='') as stream:
            writer = csv.writer(stream)
            writer.writerow(['Make', 'Selling_Price', 'Order_Date', 'Model_Year', 'Size', 'VIN'])
            for i in range(64):
                writer.writerow([('A', 'B', 'C', 'D')[i % 4], 100 + i * 1.25,
                                 f'2025-01-{1 + i % 28:02}', 2000 + i % 15, '' if i % 8 == 0 else i / 4,
                                 f'00{i:06d}'])
        subprocess.run([str(args.exe.resolve()), 'powerbi', 'export', str(source), '-o', str(project)], check=True)
    validate_project(project, args.schemas.resolve())


if __name__ == '__main__':
    main()
