//! A generated report as data: its heading, its sections' key figures,
//! notes and tables. The server keeps this as JSON and makes the HTML page
//! and each table's CSV from it.
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::Template;
use crate::time::{DAY_MS, HOUR_MS, MINUTE_MS};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub template: Template,
    pub title: String,
    pub instance_name: String,
    pub scope_label: String,
    /// The period's bounds, the end excluded, its words and the zone every
    /// time in the report is shown in.
    pub period_start: i64,
    pub period_end: i64,
    pub period_label: String,
    pub time_zone: String,
    pub generated_at: i64,
    /// What the server no longer holds, or never held, for this period.
    pub coverage: Vec<String>,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    /// Unique in the report; names its anchor.
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub figures: Vec<Figure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<Table>,
}

impl Section {
    pub fn new(id: &str, title: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            figures: Vec::new(),
            notes: Vec::new(),
            tables: Vec::new(),
        }
    }

    pub fn figure(mut self, label: &str, value: impl Into<String>, tone: Tone) -> Self {
        self.figures.push(Figure {
            label: label.to_owned(),
            value: value.into(),
            detail: None,
            tone,
        });
        self
    }

    pub fn figure_with(
        mut self,
        label: &str,
        value: impl Into<String>,
        detail: impl Into<String>,
        tone: Tone,
    ) -> Self {
        self.figures.push(Figure {
            label: label.to_owned(),
            value: value.into(),
            detail: Some(detail.into()),
            tone,
        });
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn table(mut self, table: Table) -> Self {
        self.tables.push(table);
        self
    }
}

/// A key number, such as "Devices: 412".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Figure {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub tone: Tone,
}

/// How a figure reads: neutral, good news, or something to look at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Neutral,
    Good,
    Warning,
    Bad,
}

impl Tone {
    /// Bad when `count` is above zero, good otherwise.
    pub fn bad_if(count: i64) -> Self {
        if count > 0 { Self::Bad } else { Self::Good }
    }

    pub fn warn_if(count: i64) -> Self {
        if count > 0 { Self::Warning } else { Self::Good }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    /// Unique in the report; names its CSV file.
    pub id: String,
    pub title: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    /// Rows there were before the table was cut to [`MAX_TABLE_ROWS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_rows: Option<usize>,
    /// What an empty table says.
    pub empty: String,
}

/// The most rows a table keeps; the rest are counted.
pub const MAX_TABLE_ROWS: usize = 10_000;

impl Table {
    pub fn new(id: &str, title: &str, columns: Vec<Column>, empty: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            columns,
            rows: Vec::new(),
            total_rows: None,
            empty: empty.to_owned(),
        }
    }

    /// Adds `rows`, keeping at most [`MAX_TABLE_ROWS`] and counting the rest.
    pub fn with_rows(mut self, rows: impl IntoIterator<Item = Vec<Cell>>) -> Self {
        let mut total = 0;
        for row in rows {
            total += 1;
            if self.rows.len() < MAX_TABLE_ROWS {
                debug_assert_eq!(row.len(), self.columns.len(), "{}", self.id);
                self.rows.push(row);
            }
        }
        if total > self.rows.len() {
            self.total_rows = Some(total);
        }
        self
    }

    /// The table as CSV (RFC 4180), with times in `zone`.
    pub fn csv(&self, zone: Tz) -> String {
        let mut csv = String::new();
        let header = self
            .columns
            .iter()
            .map(|column| match column.kind {
                ColumnKind::Bytes => format!("{} (GB)", column.label),
                ColumnKind::Duration => format!("{} (hours)", column.label),
                _ => column.label.clone(),
            })
            .collect::<Vec<_>>();
        push_csv_row(&mut csv, &header);
        for row in &self.rows {
            let cells = row
                .iter()
                .zip(&self.columns)
                .map(|(cell, column)| cell.csv(column.kind, zone))
                .collect::<Vec<_>>();
            push_csv_row(&mut csv, &cells);
        }
        csv
    }
}

fn push_csv_row(csv: &mut String, cells: &[String]) {
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            csv.push(',');
        }
        // A leading =, +, - or @ makes spreadsheets read a formula; a quote
        // in front keeps it text.
        let guarded = if cell.starts_with(['=', '+', '-', '@']) && cell.parse::<f64>().is_err() {
            format!("'{cell}")
        } else {
            cell.clone()
        };
        if guarded.contains([',', '"', '\n', '\r']) {
            csv.push('"');
            csv.push_str(&guarded.replace('"', "\"\""));
            csv.push('"');
        } else {
            csv.push_str(&guarded);
        }
    }
    csv.push_str("\r\n");
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub label: String,
    pub kind: ColumnKind,
}

impl Column {
    pub fn new(label: &str, kind: ColumnKind) -> Self {
        Self {
            label: label.to_owned(),
            kind,
        }
    }

    pub fn text(label: &str) -> Self {
        Self::new(label, ColumnKind::Text)
    }

    pub fn count(label: &str) -> Self {
        Self::new(label, ColumnKind::Count)
    }
}

/// How a column's cells are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    Text,
    /// A whole number.
    Count,
    /// Unix milliseconds, shown in the report's zone.
    Time,
    /// A size in bytes.
    Bytes,
    /// Milliseconds, shown as hours and minutes or days.
    Duration,
    /// A percentage.
    Percent,
}

/// One value. A missing one shows as a dash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cell {
    Missing,
    Int(i64),
    Float(f64),
    Text(String),
}

impl From<&str> for Cell {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<String> for Cell {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<i64> for Cell {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl<T: Into<Cell>> From<Option<T>> for Cell {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Missing, Into::into)
    }
}

impl Cell {
    /// The cell as the page shows it.
    pub fn display(&self, kind: ColumnKind, zone: Tz) -> String {
        match (self, kind) {
            (Self::Missing, _) => "—".to_owned(),
            (Self::Text(text), _) => text.clone(),
            (Self::Int(ms), ColumnKind::Time) => format_time(*ms, zone),
            (Self::Int(bytes), ColumnKind::Bytes) => format_bytes(*bytes),
            (Self::Int(ms), ColumnKind::Duration) => format_duration(*ms),
            (Self::Int(value), ColumnKind::Percent) => format!("{value}%"),
            (Self::Float(value), ColumnKind::Percent) => format_percent(*value),
            (Self::Int(value), _) => format_count(*value),
            (Self::Float(value), _) => format!("{value:.1}"),
        }
    }

    /// The cell in a CSV file: numbers as numbers, sizes in GB and
    /// durations in hours, times as `YYYY-MM-DD HH:MM` in the zone.
    pub fn csv(&self, kind: ColumnKind, zone: Tz) -> String {
        match (self, kind) {
            (Self::Missing, _) => String::new(),
            (Self::Text(text), _) => text.clone(),
            (Self::Int(ms), ColumnKind::Time) => format_time(*ms, zone),
            (Self::Int(bytes), ColumnKind::Bytes) => format!("{:.2}", *bytes as f64 / 1e9),
            (Self::Int(ms), ColumnKind::Duration) => format!("{:.2}", *ms as f64 / HOUR_MS as f64),
            (Self::Int(value), _) => value.to_string(),
            (Self::Float(value), _) => format!("{value:.1}"),
        }
    }

    /// A number to sort by, for numeric columns.
    pub fn sort_key(&self) -> Option<f64> {
        match self {
            Self::Int(value) => Some(*value as f64),
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }
}

/// `YYYY-MM-DD HH:MM` in `zone`.
pub fn format_time(ms: i64, zone: Tz) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .with_timezone(&zone)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Such as "3 Nov 2026", in `zone`.
pub fn format_date_ms(ms: i64, zone: Tz) -> String {
    super::period::format_date(super::period::local_date(zone, ms))
}

/// Such as "1,204".
pub fn format_count(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Decimal units, as disks are sold: "512 GB", "1.9 TB".
pub fn format_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Such as "45 min", "3 h 20 min" or "2 d 4 h".
pub fn format_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < HOUR_MS {
        let minutes = (ms + MINUTE_MS / 2) / MINUTE_MS;
        return format!("{} min", minutes.max(if ms > 0 { 1 } else { 0 }));
    }
    if ms < DAY_MS {
        let minutes = (ms + MINUTE_MS / 2) / MINUTE_MS;
        let (hours, minutes) = (minutes / 60, minutes % 60);
        return if minutes == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {minutes} min")
        };
    }
    let hours = (ms + HOUR_MS / 2) / HOUR_MS;
    let (days, hours) = (hours / 24, hours % 24);
    if hours == 0 {
        format!("{days} d")
    } else {
        format!("{days} d {hours} h")
    }
}

/// Such as "97.5%"; whole percentages without the decimal.
pub fn format_percent(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{:.0}%", value.round())
    } else {
        format!("{value:.1}%")
    }
}

/// `part` of `whole` as a percentage, or `None` of nothing.
pub fn percent(part: i64, whole: i64) -> Option<f64> {
    (whole > 0).then(|| part as f64 * 100.0 / whole as f64)
}

/// "1 device", "3 devices".
pub fn plural(count: i64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{} {noun}s", format_count(count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_as_people_write_them() {
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_count(-1200), "-1,200");
        assert_eq!(format_count(12), "12");
        assert_eq!(format_bytes(512_110_190_592), "512 GB");
        assert_eq!(format_bytes(1_920_000_000_000), "1.9 TB");
        assert_eq!(format_bytes(17_179_869_184), "17.2 GB");
        assert_eq!(format_bytes(900), "900 B");
        assert_eq!(format_duration(0), "0 min");
        assert_eq!(format_duration(20 * 1000), "1 min");
        assert_eq!(format_duration(45 * MINUTE_MS), "45 min");
        assert_eq!(format_duration(3 * HOUR_MS + 20 * MINUTE_MS), "3 h 20 min");
        assert_eq!(format_duration(2 * HOUR_MS), "2 h");
        assert_eq!(format_duration(2 * DAY_MS + 4 * HOUR_MS), "2 d 4 h");
        assert_eq!(format_percent(97.5), "97.5%");
        assert_eq!(format_percent(100.0), "100%");
        assert_eq!(percent(1, 3).map(format_percent), Some("33.3%".to_owned()));
        assert_eq!(percent(1, 0), None);
        assert_eq!(plural(1, "device"), "1 device");
        assert_eq!(plural(1200, "device"), "1,200 devices");
    }

    #[test]
    fn tables_become_csv_that_spreadsheets_read_as_text() {
        let zone: Tz = "Europe/Berlin".parse().unwrap();
        let table = Table::new(
            "devices",
            "Devices",
            vec![
                Column::text("Name"),
                Column::count("Alerts"),
                Column::new("Memory", ColumnKind::Bytes),
                Column::new("Last seen", ColumnKind::Time),
                Column::new("Resolved in", ColumnKind::Duration),
            ],
            "No devices",
        )
        .with_rows([
            vec![
                Cell::from("DESK, \"one\""),
                Cell::Int(3),
                Cell::Int(16_000_000_000),
                Cell::Int(1_767_225_600_000),
                Cell::Int(90 * MINUTE_MS),
            ],
            vec![
                Cell::from("=HYPERLINK(1)"),
                Cell::Int(-2),
                Cell::Missing,
                Cell::Missing,
                Cell::Missing,
            ],
        ]);
        assert_eq!(
            table.csv(zone),
            "Name,Alerts,Memory (GB),Last seen,Resolved in (hours)\r\n\
             \"DESK, \"\"one\"\"\",3,16.00,2026-01-01 01:00,1.50\r\n\
             '=HYPERLINK(1),-2,,,\r\n"
        );
        assert_eq!(Cell::Missing.display(ColumnKind::Count, zone), "—");
        assert_eq!(Cell::Int(1500).display(ColumnKind::Count, zone), "1,500");
        let json = serde_json::to_string(&table.rows).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<Vec<Cell>>>(&json).unwrap(),
            table.rows
        );
    }

    #[test]
    fn tables_keep_their_first_rows_and_count_the_rest() {
        let table = Table::new("t", "T", vec![Column::count("N")], "None")
            .with_rows((0..MAX_TABLE_ROWS as i64 + 5).map(|n| vec![Cell::Int(n)]));
        assert_eq!(table.rows.len(), MAX_TABLE_ROWS);
        assert_eq!(table.total_rows, Some(MAX_TABLE_ROWS + 5));
        let small =
            Table::new("t", "T", vec![Column::count("N")], "None").with_rows([vec![Cell::Int(1)]]);
        assert_eq!(small.total_rows, None);
    }
}
