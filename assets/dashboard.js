"use strict";
(() => {
    const $ = id => document.getElementById(id);
    const payload = $("report-data");
    const report = JSON.parse(payload.textContent);
    payload.remove();
    document.querySelector(".brand-sub").textContent += " · v" + report.generator_version;
    const fmt = new Intl.NumberFormat(document.documentElement.lang);
    const number = value => typeof value === "string" && /^\d+$/.test(value)
        ? fmt.format(BigInt(value)) : fmt.format(value);
    const show = value => value == null ? "—" : String(value);
    const label = value => {
        const s = String(value);
        // Full strings are retained in the profile. Limit per-cell DOM and canvas text.
        return s.length > 240 ? s.slice(0, 239) + "…" : s;
    };
    function el(tag, text, css) {
        const node = document.createElement(tag);
        if (text !== undefined) node.textContent = text;
        if (css) node.className = css;
        return node;
    }
    function btn(text, click, css) {
        const button = el("button", text, css);
        button.type = "button";
        button.addEventListener("click", click);
        return button;
    }
    function grid(headers, rows) {
        const wrap = el("div", undefined, "table-wrap");
        const table = el("table");
        const head = table.createTHead().insertRow();
        headers.forEach(name => {
            const th = el("th", label(name));
            th.scope = "col";
            head.append(th);
        });
        const body = table.createTBody();
        rows.forEach(values => {
            const tr = body.insertRow();
            values.forEach(value => {
                const td = tr.insertCell();
                if (value instanceof Node) td.append(value);
                else if (value == null) td.append(el("span", "Missing", "null"));
                else td.textContent = label(value);
            });
        });
        wrap.append(table);
        return wrap;
    }
    function pager(parent, page, pages, change, noun) {
        if (pages <= 1) return;
        const bar = el("div", undefined, "pager");
        const prev = btn("Previous", () => change(page - 1));
        const next = btn("Next", () => change(page + 1));
        prev.setAttribute("aria-label", "Previous " + noun);
        next.setAttribute("aria-label", "Next " + noun);
        prev.disabled = page === 0;
        next.disabled = page + 1 >= pages;
        bar.append(prev, el("span", `${noun} · ${number(page + 1)} of ${number(pages)}`), next);
        parent.append(bar);
    }
    function properties(entries) {
        const dl = el("dl");
        entries.forEach(([key, value]) => {
            const pair = el("div");
            pair.append(el("dt", key), el("dd", label(value)));
            dl.append(pair);
        });
        return dl;
    }
    function sumIssues(source) {
        return Object.values(source.issues || {}).reduce((total, count) => total + Number(count), 0);
    }
    function metric(value, text) {
        const box = el("div", undefined, "metric");
        box.append(el("strong", number(value)), el("span", text));
        return box;
    }
    $("metrics").append(
        metric(report.tables.reduce((n, t) => n + Number(t.rows), 0), "Accepted rows"),
        metric(report.sources.reduce((n, s) => n + Number(s.rejected_rows), 0), "Rejected rows"),
        metric(new Set(report.sources.map(s => s.path)).size, "Input files"),
        metric(report.sources.reduce((n, s) => n + Number(s.sheet != null), 0), "Worksheets"),
        metric(report.tables.length, "Logical tables"),
        metric(report.sources.reduce((n, s) => n + sumIssues(s), 0), "Audit events")
    );

    const partials = report.sources.filter(s => s.status === "partial").length;
    const skipped = report.sources.filter(s => s.status === "skipped").length;
    if (partials || skipped) {
        $("partial").hidden = false;
        $("partial").textContent = `Partial report · ${number(partials)} partially recovered, ${number(skipped)} skipped. See the source audit for details.`;
    }
    let tableIndex = -1, colIndex = 0, columnPage = 0;
    let plot = null, observer = null;
    function closeChart() {
        if (observer) observer.disconnect();
        if (plot) plot.destroy();
        observer = null;
        plot = null;
    }
    function bucketTicks(bins, numeric) {
        const labels = bins.map(bucket => bucket.label);
        if (!numeric) return {labels};

        // Numeric ranges are stored as exact "[start, end)" strings. Only the
        // axis needs a short approximation. Keep the original range elsewhere.
        const starts = labels.map(range => range.startsWith("[")
            ? range.slice(1, range.indexOf(",")) : range);
        const values = starts.map(Number);
        if (values.every((n, i) => Number.isFinite(n) && (n !== 0 || starts[i] === "0"))) {
            for (let digits = 3; digits <= 8; digits++) {
                const options = {useGrouping: false, maximumSignificantDigits: digits};
                const plain = new Intl.NumberFormat(document.documentElement.lang, options);
                const scientific = new Intl.NumberFormat(document.documentElement.lang, {...options, notation: "scientific"});
                const ticks = values.map(n => (Math.abs(n) >= 1e7 || (n !== 0 && Math.abs(n) < 0.001)
                    ? scientific : plain).format(n));
                if (new Set(ticks).size === values.length) {
                    return {labels: ticks, caption: bins.length > 1 ? "Bucket start (rounded)" : "Value (rounded)"};
                }
            }
        }
        // Very close or very large exact decimals may share a browser number.
        return {labels: labels.map((_, i) => number(i + 1)), caption: "Bucket number"};
    }
    function drawBins(parent, bins, numeric) {
        parent.append(el("h3", numeric ? "Distribution" : "Date distribution"));
        const host = el("div", undefined, "chart");
        host.setAttribute("aria-hidden", "true");
        const tools = el("div", undefined, "chart-tools");
        const hint = "Drag to zoom. Hover for the exact bucket and count.";
        const readout = el("span", hint, "chart-readout");
        const reset = btn("Reset zoom", () => plot && plot.setScale("x", {min: -0.5, max: bins.length - 0.5}));
        tools.append(reset, readout);
        parent.append(host, tools);
        const color = "#09796c";
        const x = bins.map((_, i) => i);
        const count = bins.map(item => Number(item.count));
        const {labels, caption} = bucketTicks(bins, numeric);
        const font = "12px system-ui, sans-serif";
        const measure = document.createElement("canvas").getContext("2d");
        measure.font = font;
        const labelWidth = Math.ceil(Math.max(...labels.map(text => measure.measureText(text).width)));
        const height = caption ? 225 : 200;
        function updateCursor(self) {
            const bucket = bins[self.cursor.idx];
            readout.textContent = bucket ? bucket.label + " · " + number(bucket.count) : hint;
        }
        plot = new uPlot({
            width: Math.max(1, host.clientWidth), height,
            padding: [null, Math.ceil(labelWidth / 2) + 4, null, null],
            legend: {show: false}, cursor: {drag: {x: true, y: false}},
            scales: { x: {time: false, range: (_, min, max) => [min === 0 ? -0.5 : min, max === bins.length - 1 ? max + 0.5 : max]}, y: {auto: true, range: (_, min, max) => [0, Math.max(1, max) * 1.08]} },
            series: [{}, {label: "Count", stroke: color, fill: color, paths: uPlot.paths.bars({size:[0.84, 72]}), points:{show:false}}],
            axes: [
                {font, size: 36, space: labelWidth + 12, label: caption, labelSize: 24,
                    values: (_, ticks) => ticks.map(i => labels[i] ?? "")},
                {size:68, values: (_, ticks) => ticks.map(n => number(n))}
            ],
            hooks: {setCursor: [updateCursor]}
        }, [x, count], host);
        if (typeof ResizeObserver === "function") {
            let lastWidth = host.clientWidth;
            observer = new ResizeObserver(() => {
                if (plot && host.clientWidth && host.clientWidth !== lastWidth) {
                    lastWidth = host.clientWidth;
                    plot.setSize({width:lastWidth, height});
                }
            });
            observer.observe(host);
        }
        const details = el("details");
        details.append(el("summary", "Bucket counts"), grid(["Bucket", "Count"], bins.map(b => [el("span", b.label), number(b.count)])));
        parent.append(details);
    }
    const active = () => report.tables[tableIndex];
    function renderDetail() {
        closeChart();
        const parent = $("detail");
        parent.replaceChildren();
        const t = active();
        const c = t?.profile.columns[colIndex];
        if (!c) { parent.append(el("p", "No columns are available in this table.")); return; }
        const head = el("div", undefined, "detail-head");
        head.append(el("h2", label(c.name)), el("span", c.kind, "tag"));
        parent.append(head);
        const pairs = [["Valid", number(c.valid)], ["Missing", number(c.missing)], ["Distinct", number(c.distinct)]];
        if (Number(t.rows)) pairs.push(["Complete", ((Number(c.valid) / Number(t.rows)) * 100).toFixed(1) + "%"]);
        if (c.min != null) pairs.push(["Minimum", c.min]);
        if (c.max != null) pairs.push(["Maximum", c.max]);
        if (c.mean != null) pairs.push(["Mean", String(c.mean)]);
        parent.append(properties(pairs));
        const missing = Object.entries(c.missing_reasons || {});
        if (missing.length) parent.append(el("h3", "Missing values"), grid(["Reason", "Count"], missing.map(([key,n]) => [key, number(n)])));
        if (c.bins?.length) drawBins(parent, c.bins, c.kind === "integer" || c.kind === "decimal");
        if (c.top?.length) {
            parent.append(el("h3", "Most frequent values"));
            const rows = c.top.map(item => [item.value, number(item.count)]);
            if (Number(c.other) > 0) rows.push([el("em", "All other values"), number(c.other)]);
            parent.append(grid(["Value", "Count"], rows));
        }
        if (!Number(c.valid)) parent.append(el("p", "No valid values to chart.", "sub"));
    }
    function renderColumns() {
        const box = $("columns"), pages = $("column-pages");
        box.replaceChildren(); pages.replaceChildren();
        const columns = active()?.profile.columns || [];
        const search = $("column-search").value.toLocaleLowerCase();
        const indices = [];
        columns.forEach((col,i) => { if (col.name.toLocaleLowerCase().includes(search)) indices.push(i); });
        const size = 35, count = Math.ceil(indices.length / size);
        indices.slice(columnPage * size, (columnPage + 1) * size).forEach(i => {
            const col = columns[i], link = btn(undefined, () => { colIndex = i; renderColumns(); renderDetail(); }, "col");
            link.setAttribute("aria-pressed", String(colIndex === i));
            link.append(el("span", label(col.name)), el("span", col.kind, "quiet"));
            box.append(link);
        });
        if (!indices.length) box.append(el("p", "No matching columns.", "sub"));
        pager(pages, columnPage, count, p => {columnPage=p;renderColumns();}, "Column pages");
    }
    $("column-search").addEventListener("input", () => { columnPage = 0; renderColumns(); });
    function renderSample(rowPage=0, fieldsPage=0) {
        const t = active(), box = $("sample");
        box.replaceChildren();
        const samples = t.profile.sample || [];
        $("sample-count").textContent = `${number(samples.length)} shown from ${number(t.rows)} accepted rows`;
        if (t.sample_omitted) { box.append(el("p", "The report size limit excluded this table’s sample.", "notice")); return; }
        if (!samples.length) { box.append(el("p", "No sample rows are available.", "sub")); return; }
        const fields = t.profile.columns;
        const cols = Math.min(fields.length, Math.max(...samples.map(row => row.values.length)));
        const perRow = 15, perCol = 10, begin = fieldsPage * perCol, end = Math.min(cols, begin + perCol);
        if (cols < fields.length) box.append(el("p", `Sample includes the first ${number(cols)} of ${number(fields.length)} columns.`, "sub"));
        const names = fields.slice(begin, end).map(c => c.name);
        const rows = samples.slice(rowPage * perRow, (rowPage + 1) * perRow).map(r => [
            r.source, number(r.row), ...Array.from({length: end-begin}, (_,i) => r.values[begin+i] ?? null)
        ]);
        box.append(grid(["Source", "Source row", ...names], rows));
        pager(box, rowPage, Math.ceil(samples.length/perRow), p => renderSample(p,fieldsPage), "Row pages");
        pager(box, fieldsPage, Math.ceil(cols/perCol), p => renderSample(rowPage,p), "Column pages");
    }
    function chooseTable() {
        closeChart();
        if (active()) delete active().profile;
        tableIndex = Number($("tables").value);
        if (active()) active().profile = JSON.parse($(active().profile_id).textContent);
        colIndex = 0; columnPage = 0; $("column-search").value = "";
        const t = active();
        $("table-summary").textContent = t ? `${number(t.rows)} rows · ${number(t.columns)} columns` : "No tables were accepted. See the source audit for details.";
        if (t?.clean_file) $("table-summary").textContent += " · Clean export: " + t.clean_file;
        $("profile-body").hidden = !t;
        if (t) { renderColumns(); renderDetail(); renderSample(); }
    }
    report.tables.forEach((t,i) => { const o=el("option", label(t.name)); o.value=String(i); $("tables").append(o); });
    $("tables").addEventListener("change", chooseTable);

    function renderSources(page=0) {
        const parent = $("sources"), size = 20;
        parent.replaceChildren();
        if (report.examples_omitted) parent.append(el("p", "The report size limit excluded audit examples. All event counts are included.", "notice"));
        report.sources.slice(page*size,(page+1)*size).forEach(s => {
            const details = el("details");
            const name = s.sheet == null ? s.path : s.path + " / " + s.sheet;
            details.append(el("summary", `${label(name)} · ${s.status} · ${number(s.rows)} rows · ${number(sumIssues(s))} audit events`));
            details.addEventListener("toggle", () => {
                if (!details.open || details.childElementCount > 1) return;
                details.append(properties([["Source", name],["Table",show(s.table)],["Accepted rows",number(s.rows)],["Rejected rows",number(s.rejected_rows)],["Layout",show(s.layout)],["Encoding",show(s.encoding)]]));
                if (s.rules) {
                    const rules = el("details");
                    const code = Object.keys(s.rules).length ? el("pre", JSON.stringify(s.rules, null, 2)) : el("p", "Automatic defaults", "sub");
                    rules.append(el("summary", "Effective source rules"), code);
                    details.append(rules);
                }
                if (s.message) details.append(el("p", label(s.message), "notice"));
                const issues = Object.entries(s.issues);
                if (issues.length) details.append(el("h3","Issue counts"), grid(["Issue","Count"],issues.map(([k,n]) => [k,number(n)])));
                if (s.examples.length) details.append(el("h3","Recorded examples"), grid(["Row","Lines","Column","Issue","Detail","Excerpt"], s.examples.map(e => [
                    e.row || "—", e.line_start ? e.line_start === e.line_end ? e.line_start : e.line_start + "–" + e.line_end : "—",
                    e.col == null ? "—" : e.col + 1, e.code, e.message, e.excerpt
                ])));
            });
            parent.append(details);
        });
        if (!report.sources.length) parent.append(el("p", "No source entries.", "sub"));
        pager(parent,page,Math.ceil(report.sources.length/size),renderSources,"Source pages");
    }
    function view(sources) {
        $("profile-view").hidden = sources; $("source-view").hidden = !sources;
        $("view-profile").setAttribute("aria-pressed", String(!sources));
        $("view-sources").setAttribute("aria-pressed", String(sources));
        if (sources) { closeChart(); renderSources(); }
        else if (active()) renderDetail();
    }
    $("view-profile").addEventListener("click", () => view(false));
    $("view-sources").addEventListener("click", () => view(true));
    chooseTable();
})();
