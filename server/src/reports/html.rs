//! A report as one self-contained HTML page: its styles and its one script
//! are inline and it loads nothing, so it reads the same served by MeshRMM,
//! attached to an email or saved to disk. It follows the reader's light or
//! dark setting, and prints with each section on a new page, which is how
//! it becomes a PDF ("Save as PDF").
//!
//! Each table has a button that downloads it as CSV, made in the page from
//! the table itself: cells whose CSV value differs from what they show (a
//! size in GB, a time without its zone) carry it in `data-v`.
use std::{fmt::Write, sync::LazyLock};

use base64::Engine as _;
use chrono_tz::Tz;
use sha2::{Digest, Sha256};

use super::document::{Cell, ColumnKind, Document, Figure, Section, Table, Tone, format_time};

const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #ffffff; --fg: #17202b; --muted: #5b6573; --line: #dde2e8; --soft: #f4f6f8;
  --accent: #2459c4; --good: #17803d; --warning: #a35d00; --bad: #c0262d;
  --good-bg: #e9f6ee; --warning-bg: #fdf3e2; --bad-bg: #fcebec;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #12161c; --fg: #e6e9ee; --muted: #9aa4b2; --line: #2c333d; --soft: #1a2029;
    --accent: #7ea6ff; --good: #5fcf86; --warning: #f2b45c; --bad: #ff7b7f;
    --good-bg: #15291d; --warning-bg: #2e2412; --bad-bg: #33181a;
  }
}
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body {
  margin: 0; background: var(--bg); color: var(--fg);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
}
.page { max-width: 1180px; margin: 0 auto; padding: 32px 24px 48px; }
header.report { border-bottom: 1px solid var(--line); padding-bottom: 20px; margin-bottom: 24px; }
.instance { margin: 0; color: var(--muted); font-weight: 600; letter-spacing: .02em; }
h1 { margin: 4px 0 16px; font-size: 30px; line-height: 1.2; }
h2 { margin: 0 0 14px; font-size: 22px; }
h3 { margin: 0; font-size: 16px; }
dl.meta { display: flex; flex-wrap: wrap; gap: 12px 40px; margin: 0; }
dl.meta div { min-width: 0; }
dl.meta dt { color: var(--muted); font-size: 12px; text-transform: uppercase; letter-spacing: .06em; }
dl.meta dd { margin: 2px 0 0; font-weight: 600; }
nav.contents ol { display: flex; flex-wrap: wrap; gap: 6px 18px; margin: 0 0 20px; padding: 0; list-style: none; }
nav.contents a { color: var(--accent); text-decoration: none; }
nav.contents a:hover { text-decoration: underline; }
aside.coverage { background: var(--soft); border: 1px solid var(--line); border-radius: 8px; padding: 12px 16px; margin-bottom: 28px; color: var(--muted); font-size: 14px; }
aside.coverage h2 { font-size: 14px; margin: 0 0 4px; color: var(--fg); }
aside.coverage ul { margin: 0; padding-left: 18px; }
section.part { margin: 0 0 40px; }
.figures { display: grid; grid-template-columns: repeat(auto-fill, minmax(170px, 1fr)); gap: 12px; margin-bottom: 16px; }
.figure { border: 1px solid var(--line); border-radius: 8px; padding: 12px 14px; display: flex; flex-direction: column; gap: 2px; }
.figure .label { color: var(--muted); font-size: 13px; }
.figure .value { font-size: 24px; font-weight: 650; line-height: 1.25; font-variant-numeric: tabular-nums; }
.figure .detail { color: var(--muted); font-size: 12px; }
.figure.good { border-color: color-mix(in srgb, var(--good) 35%, var(--line)); background: var(--good-bg); }
.figure.good .value { color: var(--good); }
.figure.warning { border-color: color-mix(in srgb, var(--warning) 35%, var(--line)); background: var(--warning-bg); }
.figure.warning .value { color: var(--warning); }
.figure.bad { border-color: color-mix(in srgb, var(--bad) 35%, var(--line)); background: var(--bad-bg); }
.figure.bad .value { color: var(--bad); }
p.note { color: var(--muted); font-size: 13px; margin: 0 0 14px; max-width: 80ch; }
.block { margin: 18px 0 0; }
.block-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 8px; }
button.csv { font: inherit; font-size: 13px; color: var(--accent); background: transparent; border: 1px solid var(--line); border-radius: 6px; padding: 4px 10px; cursor: pointer; }
button.csv:hover { background: var(--soft); }
.scroll { overflow-x: auto; border: 1px solid var(--line); border-radius: 8px; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th, td { padding: 7px 10px; text-align: left; vertical-align: top; border-bottom: 1px solid var(--line); }
th { background: var(--soft); font-weight: 600; white-space: nowrap; }
tbody tr:last-child td { border-bottom: 0; }
td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
td.time { white-space: nowrap; }
p.empty { color: var(--muted); font-style: italic; margin: 0; padding: 10px 0; }
p.cut { color: var(--muted); font-size: 12px; margin: 6px 0 0; }
footer { color: var(--muted); font-size: 12px; border-top: 1px solid var(--line); padding-top: 12px; margin-top: 24px; }
@media (max-width: 600px) {
  .page { padding: 20px 16px 32px; }
  h1 { font-size: 24px; }
}
@media print {
  :root {
    color-scheme: light;
    --bg: #ffffff; --fg: #000000; --muted: #444b55; --line: #c9ced6; --soft: #f1f3f5;
    --accent: #1d4fb8; --good: #13692f; --warning: #8a4f00; --bad: #a51f25;
    --good-bg: #ffffff; --warning-bg: #ffffff; --bad-bg: #ffffff;
  }
  @page { margin: 14mm 12mm; }
  body { font-size: 10pt; }
  .page { max-width: none; padding: 0; }
  nav.contents, button.csv { display: none; }
  section.part { break-before: page; margin: 0; }
  .figures { grid-template-columns: repeat(4, 1fr); }
  .figure { break-inside: avoid; }
  .scroll { overflow: visible; border: 0; }
  table { font-size: 8pt; }
  th, td { padding: 3px 5px; }
  th { white-space: normal; }
  thead { display: table-header-group; }
  tr { break-inside: avoid; }
  .block-head { break-after: avoid; }
  a { color: inherit; text-decoration: none; }
}
"#;

/// Downloads a table as CSV, as the server's CSV route writes it.
const SCRIPT: &str = r#"(function () {
  function quote(value) {
    if (/^[=+\-@]/.test(value) && isNaN(Number(value))) value = "'" + value;
    return /[",\r\n]/.test(value) ? '"' + value.replace(/"/g, '""') + '"' : value;
  }
  document.addEventListener("click", function (event) {
    var button = event.target.closest && event.target.closest("button[data-table]");
    if (!button) return;
    var table = document.getElementById(button.getAttribute("data-table"));
    if (!table) return;
    var lines = [];
    for (var r = 0; r < table.rows.length; r++) {
      var cells = table.rows[r].cells, line = [];
      for (var c = 0; c < cells.length; c++) {
        var cell = cells[c];
        line.push(quote(cell.hasAttribute("data-v") ? cell.getAttribute("data-v") : cell.textContent));
      }
      lines.push(line.join(","));
    }
    var blob = new Blob([lines.join("\r\n") + "\r\n"], { type: "text/csv;charset=utf-8" });
    var link = document.createElement("a");
    link.href = URL.createObjectURL(blob);
    link.download = button.getAttribute("data-file");
    document.body.appendChild(link);
    link.click();
    link.remove();
    setTimeout(function () { URL.revokeObjectURL(link.href); }, 10000);
  });
})();"#;

/// The Content-Security-Policy the server sends a report with: its own
/// styles and script only, and nothing loaded from anywhere.
pub static CONTENT_SECURITY_POLICY: LazyLock<String> = LazyLock::new(|| {
    let hash = |text: &str| {
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(text.as_bytes()))
    };
    format!(
        "default-src 'none'; style-src 'sha256-{}'; script-src 'sha256-{}'; img-src data:; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        hash(STYLE),
        hash(SCRIPT)
    )
});

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// A name for files made from the report, such as
/// `executive-summary-2026-11-02`.
pub fn file_stem(document: &Document) -> String {
    let zone = zone_of(document);
    let date = &format_time(document.generated_at, zone)[..10];
    format!("{}-{date}", document.template.as_str().replace('_', "-"))
}

pub fn zone_of(document: &Document) -> Tz {
    document.time_zone.parse().unwrap_or(Tz::UTC)
}

/// The report as an HTML page.
pub fn render(document: &Document) -> String {
    let zone = zone_of(document);
    let stem = file_stem(document);
    let mut html = String::with_capacity(64 * 1024);
    let _ = write!(
        html,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"light dark\">\n\
         <meta name=\"generator\" content=\"MeshRMM\">\n\
         <title>{title} · {instance}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<div class=\"page\">\n",
        title = escape(&document.title),
        instance = escape(&document.instance_name),
    );
    let _ = write!(
        html,
        "<header class=\"report\">\n<p class=\"instance\">{instance}</p>\n<h1>{title}</h1>\n<dl class=\"meta\">\n\
         <div><dt>Devices</dt><dd>{scope}</dd></div>\n\
         <div><dt>Period</dt><dd>{period}</dd></div>\n\
         <div><dt>Time zone</dt><dd>{zone}</dd></div>\n\
         <div><dt>Generated</dt><dd>{generated}</dd></div>\n</dl>\n</header>\n",
        instance = escape(&document.instance_name),
        title = escape(&document.title),
        scope = escape(&document.scope_label),
        period = escape(&document.period_label),
        zone = escape(&document.time_zone),
        generated = escape(&format_time(document.generated_at, zone)),
    );
    if document.sections.len() > 1 {
        html.push_str("<nav class=\"contents\" aria-label=\"Contents\"><ol>");
        for section in &document.sections {
            let _ = write!(
                html,
                "<li><a href=\"#s-{id}\">{title}</a></li>",
                id = escape(&section.id),
                title = escape(&section.title)
            );
        }
        html.push_str("</ol></nav>\n");
    }
    if !document.coverage.is_empty() {
        html.push_str("<aside class=\"coverage\">\n<h2>What this report covers</h2>\n<ul>\n");
        for note in &document.coverage {
            let _ = writeln!(html, "<li>{}</li>", escape(note));
        }
        html.push_str("</ul>\n</aside>\n");
    }
    html.push_str("<main>\n");
    for section in &document.sections {
        render_section(&mut html, section, zone, &stem);
    }
    html.push_str("</main>\n");
    let _ = write!(
        html,
        "<footer>Made by MeshRMM for {instance}. Times are in {zone}.</footer>\n</div>\n<script>{SCRIPT}</script>\n</body>\n</html>\n",
        instance = escape(&document.instance_name),
        zone = escape(&document.time_zone),
    );
    html
}

fn render_section(html: &mut String, section: &Section, zone: Tz, stem: &str) {
    let _ = write!(
        html,
        "<section class=\"part\" id=\"s-{id}\">\n<h2>{title}</h2>\n",
        id = escape(&section.id),
        title = escape(&section.title)
    );
    if !section.figures.is_empty() {
        html.push_str("<div class=\"figures\">\n");
        for figure in &section.figures {
            render_figure(html, figure);
        }
        html.push_str("</div>\n");
    }
    for note in &section.notes {
        let _ = writeln!(html, "<p class=\"note\">{}</p>", escape(note));
    }
    for table in &section.tables {
        render_table(html, &section.id, table, zone, stem);
    }
    html.push_str("</section>\n");
}

fn render_figure(html: &mut String, figure: &Figure) {
    let tone = match figure.tone {
        Tone::Neutral => "",
        Tone::Good => " good",
        Tone::Warning => " warning",
        Tone::Bad => " bad",
    };
    let _ = write!(
        html,
        "<div class=\"figure{tone}\"><span class=\"label\">{label}</span><span class=\"value\">{value}</span>",
        label = escape(&figure.label),
        value = escape(&figure.value)
    );
    if let Some(detail) = &figure.detail {
        let _ = write!(html, "<span class=\"detail\">{}</span>", escape(detail));
    }
    html.push_str("</div>\n");
}

fn numeric(kind: ColumnKind) -> bool {
    matches!(
        kind,
        ColumnKind::Count | ColumnKind::Bytes | ColumnKind::Duration | ColumnKind::Percent
    )
}

fn render_table(html: &mut String, section_id: &str, table: &Table, zone: Tz, stem: &str) {
    let id = format!("t-{section_id}-{}", table.id);
    let _ = write!(
        html,
        "<div class=\"block\">\n<div class=\"block-head\"><h3>{title}</h3>",
        title = escape(&table.title)
    );
    if table.rows.is_empty() {
        let _ = write!(
            html,
            "</div>\n<p class=\"empty\">{}</p>\n</div>\n",
            escape(&table.empty)
        );
        return;
    }
    let _ = write!(
        html,
        "<button type=\"button\" class=\"csv\" data-table=\"{id}\" data-file=\"{file}\">Download CSV</button></div>\n\
         <div class=\"scroll\"><table id=\"{id}\">\n<thead><tr>",
        id = escape(&id),
        file = escape(&format!("{stem}-{}.csv", table.id)),
    );
    for column in &table.columns {
        let class = if numeric(column.kind) {
            " class=\"num\""
        } else {
            ""
        };
        let csv = match column.kind {
            ColumnKind::Bytes => Some(format!("{} (GB)", column.label)),
            ColumnKind::Duration => Some(format!("{} (hours)", column.label)),
            _ => None,
        };
        let _ = write!(html, "<th scope=\"col\"{class}");
        if let Some(csv) = csv {
            let _ = write!(html, " data-v=\"{}\"", escape(&csv));
        }
        let _ = write!(html, ">{}</th>", escape(&column.label));
    }
    html.push_str("</tr></thead>\n<tbody>\n");
    for row in &table.rows {
        html.push_str("<tr>");
        for (cell, column) in row.iter().zip(&table.columns) {
            render_cell(html, cell, column.kind, zone);
        }
        html.push_str("</tr>\n");
    }
    html.push_str("</tbody></table></div>\n");
    if let Some(total) = table.total_rows {
        let _ = writeln!(
            html,
            "<p class=\"cut\">The first {} of {} rows.</p>",
            super::document::format_count(table.rows.len() as i64),
            super::document::format_count(total as i64)
        );
    }
    html.push_str("</div>\n");
}

fn render_cell(html: &mut String, cell: &Cell, kind: ColumnKind, zone: Tz) {
    let display = cell.display(kind, zone);
    let csv = cell.csv(kind, zone);
    let class = if numeric(kind) {
        " class=\"num\""
    } else if kind == ColumnKind::Time {
        " class=\"time\""
    } else {
        ""
    };
    let _ = write!(html, "<td{class}");
    if csv != display {
        let _ = write!(html, " data-v=\"{}\"", escape(&csv));
    }
    let _ = write!(html, ">{}</td>", escape(&display));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reports::{
        Template,
        document::{Column, Section, Table},
    };

    fn document(sections: Vec<Section>) -> Document {
        Document {
            template: Template::Alerts,
            title: "Alerts".into(),
            instance_name: "Acme <IT>".into(),
            scope_label: "Every device".into(),
            period_start: 0,
            period_end: 1,
            period_label: "1 Oct 2026 – 31 Oct 2026".into(),
            time_zone: "Europe/Berlin".into(),
            generated_at: 1_793_000_000_000,
            coverage: vec!["Resolved alerts are kept for 90 days.".into()],
            sections,
        }
    }

    #[test]
    fn pages_escape_everything_and_carry_csv_values() {
        let section = Section::new("devices", "Devices")
            .figure("Raised", "3", Tone::Bad)
            .table(
                Table::new(
                    "by-device",
                    "By device",
                    vec![
                        Column::text("Device"),
                        Column::new("Memory", ColumnKind::Bytes),
                    ],
                    "None",
                )
                .with_rows([vec![
                    Cell::from("<script>alert(1)</script>"),
                    Cell::Int(16_000_000_000),
                ]]),
            )
            .table(Table::new(
                "empty",
                "Empty",
                vec![Column::text("A")],
                "Nothing here",
            ));
        let html = render(&document(vec![section]));
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("Acme &lt;IT&gt;"));
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("<td class=\"num\" data-v=\"16.00\">16.0 GB</td>"));
        assert!(
            html.contains("<th scope=\"col\" class=\"num\" data-v=\"Memory (GB)\">Memory</th>")
        );
        assert!(html.contains("data-file=\"alerts-2026-10-26-by-device.csv\""));
        assert!(html.contains("<p class=\"empty\">Nothing here</p>"));
        assert!(html.contains("@media print"));
        assert!(html.contains("prefers-color-scheme: dark"));
        // Nothing is loaded from anywhere.
        for external in ["http://", "https://", "src=", "<link"] {
            assert!(!html.contains(external), "{external}");
        }
    }

    #[test]
    fn the_policy_allows_the_pages_own_style_and_script_only() {
        let policy = CONTENT_SECURITY_POLICY.as_str();
        assert!(policy.starts_with("default-src 'none'; style-src 'sha256-"));
        assert!(!policy.contains("unsafe-inline"));
        let html = render(&document(vec![]));
        assert_eq!(html.matches("<style>").count(), 1);
        assert_eq!(html.matches("<script>").count(), 1);
    }
}
