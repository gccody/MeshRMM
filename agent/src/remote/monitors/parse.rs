//! Reading what the operating system's tools and logs say, for monitors.
//! Everything here is plain text handling, so it is tested on every
//! platform with output captured from the real tools (`testdata/`).
use meshrmm_protocol::EventLevel;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day since 1970-01-01.
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `2026-11-09`.
pub(crate) fn format_date(unix_ms: i64) -> String {
    let (year, month, day) = civil_from_days(unix_ms.div_euclid(DAY_MS));
    format!("{year:04}-{month:02}-{day:02}")
}

/// `2026-11-09 09:38:10 UTC`.
pub(crate) fn format_time(unix_ms: i64) -> String {
    let seconds = unix_ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}:{:02} UTC",
        format_date(unix_ms),
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn number(text: &str) -> Option<i64> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// Unix milliseconds of `2026-11-09T09:38:10.5016551Z` (the Windows Event
/// Log's) or `2026-11-09 09:38:10.123456-0800` (the macOS unified log's).
/// Logs hold what programs wrote, so text that isn't a time, in any
/// characters, is `None`.
pub(crate) fn parse_timestamp(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.len() < 19 {
        return None;
    }
    let (date, rest) = text.split_at_checked(10)?;
    let mut parts = date.split('-');
    let (year, month, day) = (
        number(parts.next()?)?,
        number(parts.next()?)?,
        number(parts.next()?)?,
    );
    let rest = rest.strip_prefix(['T', ' '])?;
    let (clock, mut zone) = rest.split_at_checked(8)?;
    let mut clock_parts = clock.split(':');
    let (hour, minute, second) = (
        number(clock_parts.next()?)?,
        number(clock_parts.next()?)?,
        number(clock_parts.next()?)?,
    );
    let mut millis = 0;
    if let Some(fraction) = zone.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        let padded = format!("{:0<3}", fraction.get(..digits.min(3))?);
        millis = number(&padded)?;
        zone = fraction.get(digits..)?;
    }
    let offset_minutes = match zone {
        "" | "Z" => 0,
        _ => {
            let (sign, digits) = if let Some(digits) = zone.strip_prefix('+') {
                (1, digits)
            } else {
                (-1, zone.strip_prefix('-')?)
            };
            let digits = digits.replace(':', "");
            if digits.len() != 4 {
                return None;
            }
            let (hours, minutes) = digits.split_at_checked(2)?;
            sign * (number(hours)? * 60 + number(minutes)?)
        }
    };
    let days = days_from_civil(year, month, day);
    Some(
        ((days * 86_400 + hour * 3600 + minute * 60 + second) - offset_minutes * 60) * 1000
            + millis,
    )
}

/// One event from a log.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LogEvent {
    pub(crate) at_ms: i64,
    pub(crate) level: Option<EventLevel>,
    /// The names a monitor's source can match: the Windows provider, the
    /// systemd unit, syslog identifier or command, or the macOS subsystem
    /// and process.
    pub(crate) sources: Vec<String>,
    pub(crate) event_id: Option<u32>,
    pub(crate) message: String,
}

/// What one check read from a log.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LogRead {
    pub(crate) events: Vec<LogEvent>,
    /// A read newest first (the journal, the unified log) stopped at its
    /// cap, so older entries since the last check went unread.
    pub(crate) truncated: bool,
    /// A read oldest first (a Windows log) stopped part way; the next check
    /// reads on from where it got to.
    pub(crate) behind: Option<Behind>,
    /// The check had fallen too far behind, so it skipped the entries
    /// logged from `.0` to `.1` and read on from `.1`.
    pub(crate) skipped: Option<(i64, i64)>,
    /// The record the last check got to is gone because the log was
    /// cleared, so the read went by time again, from then.
    pub(crate) cleared_since_ms: Option<i64>,
    /// Records after the one the last check got to were overwritten by the
    /// log (it was full) before they were read: how many.
    pub(crate) overwritten: u64,
}

/// Where a read that stopped part way got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Behind {
    /// At the check's time limit; else at the cap on matching entries.
    pub(crate) out_of_time: bool,
    /// The time of the last entry it read, which entries logged after it
    /// follow.
    pub(crate) read_to_ms: i64,
    /// How long before the check started that was.
    pub(crate) lag_ms: i64,
}

/// What a log read starts from when it has no place of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LogWindow {
    /// When the check started.
    pub(crate) now_ms: i64,
    /// Since the last check, or one interval back on the first.
    pub(crate) since_ms: i64,
    pub(crate) interval_ms: i64,
}

/// How many intervals a monitor's checks may fall behind a log: one that
/// stopped part way reads on from where it got to, unless that is longer
/// ago than this, when it skips to the last interval. A burst gets two
/// more checks to catch up in before anything is skipped, and a log too
/// busy ever to keep up with raises its alerts at most this late.
pub(crate) const BEHIND_INTERVALS: i64 = 3;

/// How far a monitor read a Windows log, which its next check reads on
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LogPlace {
    /// The last record read; with none, the next check reads by time.
    pub(crate) record: Option<u64>,
    /// Everything logged before this was read: the check's start when it
    /// read to the end, else the time of the last entry it read.
    pub(crate) read_to_ms: i64,
    /// The check stopped before the end.
    pub(crate) behind: bool,
}

/// An event log monitor's filters.
#[derive(Debug, Clone, Default)]
pub(crate) struct EventFilter {
    pub(crate) source: String,
    pub(crate) event_ids: Vec<u32>,
    pub(crate) levels: Vec<EventLevel>,
    pub(crate) message: String,
}

impl EventFilter {
    /// Whether the filter's source names `source`, ignoring case, and
    /// without a unit's `.service`.
    fn names(&self, source: &str) -> bool {
        source.eq_ignore_ascii_case(&self.source)
            || source
                .strip_suffix(".service")
                .is_some_and(|unit| unit.eq_ignore_ascii_case(&self.source))
    }

    /// The name to show as `event`'s source: the one the filter matched,
    /// else its first.
    pub(crate) fn source_of<'a>(&self, event: &'a LogEvent) -> Option<&'a str> {
        event
            .sources
            .iter()
            .find(|source| !self.source.is_empty() && self.names(source))
            .or_else(|| event.sources.first())
            .map(String::as_str)
    }

    /// Whether `event` passes every filter that is set. Sources compare
    /// ignoring case, as does the message substring.
    pub(crate) fn matches(&self, event: &LogEvent) -> bool {
        (self.source.is_empty() || event.sources.iter().any(|source| self.names(source)))
            && (self.event_ids.is_empty()
                || event
                    .event_id
                    .is_some_and(|id| self.event_ids.contains(&id)))
            && (self.levels.is_empty()
                || event
                    .level
                    .is_some_and(|level| self.levels.contains(&level)))
            && (self.message.is_empty()
                || event
                    .message
                    .to_lowercase()
                    .contains(&self.message.to_lowercase()))
    }
}

/// XML text with its five entities and character references decoded.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        let Some((entity, next)) = after.split_once(';') else {
            out.push('&');
            rest = after;
            break;
        };
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .map(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').map(|digits| digits.parse().ok()))
                .flatten()
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = next;
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The value of `name="…"` or `name='…'` in a tag's text.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let mut search = tag;
    loop {
        let (before, after) = search.split_once(name)?;
        let boundary = before
            .chars()
            .next_back()
            .is_none_or(|c| c.is_ascii_whitespace());
        if boundary && let Some(value) = after.strip_prefix('=') {
            for quote in ['\'', '"'] {
                if let Some(value) = value.strip_prefix(quote) {
                    return Some(unescape(value.split_once(quote)?.0));
                }
            }
        }
        search = after;
    }
}

/// The first `<name …>` start tag's text (between `<` and `>`), and where
/// the element's content begins.
fn start_tag<'a>(xml: &'a str, name: &str) -> Option<(&'a str, usize)> {
    let mut from = 0;
    loop {
        let at = from + xml.get(from..)?.find(&format!("<{name}"))?;
        let after = at + 1 + name.len();
        let rest = xml.get(after..)?;
        if rest.starts_with(['>', ' ', '/', '\t', '\r', '\n']) {
            let end = after + rest.find('>')?;
            return Some((xml.get(at + 1..end)?, end + 1));
        }
        from = after;
    }
}

/// The text of the first `<name>` element, decoded.
fn element_text(xml: &str, name: &str) -> Option<String> {
    let (tag, content) = start_tag(xml, name)?;
    if tag.ends_with('/') {
        return Some(String::new());
    }
    let (text, _) = xml.get(content..)?.split_once(&format!("</{name}>"))?;
    Some(unescape(text))
}

/// The provider named in a Windows event's XML, whose publisher formats
/// its message.
pub(crate) fn windows_provider(xml: &str) -> Option<String> {
    let (provider, _) = start_tag(xml, "Provider")?;
    attribute(provider, "Name")
}

fn windows_level(level: u8) -> Option<EventLevel> {
    EventLevel::ALL
        .into_iter()
        .find(|candidate| candidate.windows_levels().contains(&level))
}

/// An event in the Windows Event Log's XML. Its message is the rendered one
/// when the XML has it (`RenderingInfo`), else `message`, else its data.
pub(crate) fn windows_event(xml: &str, message: Option<String>) -> Option<LogEvent> {
    let system_end = xml.find("</System>")?;
    let system = xml.get(..system_end)?;
    let (provider, _) = start_tag(system, "Provider")?;
    let mut sources = Vec::new();
    for name in ["Name", "EventSourceName"] {
        if let Some(value) = attribute(provider, name)
            && !value.is_empty()
            && !sources.contains(&value)
        {
            sources.push(value);
        }
    }
    let event_id = element_text(system, "EventID").and_then(|id| id.trim().parse().ok());
    let level = element_text(system, "Level")
        .and_then(|level| level.trim().parse::<u8>().ok())
        .and_then(windows_level);
    let (time, _) = start_tag(system, "TimeCreated")?;
    let at_ms = parse_timestamp(&attribute(time, "SystemTime")?)?;
    let rendered = start_tag(xml, "RenderingInfo")
        .and_then(|(_, content)| element_text(xml.get(content..)?, "Message"));
    let message = rendered.or(message).unwrap_or_else(|| {
        let mut data = Vec::new();
        let mut rest = xml.get(system_end..).unwrap_or_default();
        while let Some((tag, content)) = start_tag(rest, "Data") {
            let Some(inside) = rest.get(content..) else {
                break;
            };
            if tag.ends_with('/') {
                rest = inside;
                continue;
            }
            let Some((value, after)) = inside.split_once("</Data>") else {
                break;
            };
            let value = unescape(value);
            if !value.is_empty() {
                data.push(value);
            }
            rest = after;
        }
        data.join("; ")
    });
    Some(LogEvent {
        at_ms,
        level,
        sources,
        event_id,
        message: message.trim().to_owned(),
    })
}

/// Every `<Event>` in `xml`, as `wevtutil qe … /f:RenderedXml` writes them.
#[cfg(test)]
pub(crate) fn windows_events(xml: &str) -> Vec<LogEvent> {
    xml.split("</Event>")
        .filter_map(|part| windows_event(part, None))
        .collect()
}

/// The number of the Windows event's record, which its log counts up as it
/// writes them.
pub(crate) fn windows_record(xml: &str) -> Option<u64> {
    let system = xml.get(..xml.find("</System>")?)?;
    element_text(system, "EventRecordID")?.trim().parse().ok()
}

/// Which records of a Windows log a query selects: those numbered up to
/// `through`, and after `after`, or with none, logged within the last
/// `age_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Records {
    pub(crate) after: Option<u64>,
    pub(crate) through: u64,
    pub(crate) age_ms: u64,
}

/// `terms` joined by `or`, grouped in halves so the expression nests only
/// as deep as their count's logarithm: the Windows Event Log refuses a
/// query that nests deeper than 23 (`ERROR_EVT_INVALID_QUERY`), and each
/// `or` in a plain list nests one deeper.
fn any_of(terms: &[String]) -> String {
    match terms {
        [] => String::new(),
        [term] => term.clone(),
        _ => {
            let (first, second) = terms.split_at(terms.len() / 2);
            let half = |terms: &[String]| {
                if terms.len() == 1 {
                    any_of(terms)
                } else {
                    format!("({})", any_of(terms))
                }
            };
            format!("{} or {}", half(first), half(second))
        }
    }
}

/// The XPath query the Windows Event Log filters by, for `records`. The
/// source was checked to hold no quote.
pub(crate) fn windows_query(filter: &EventFilter, records: Records) -> String {
    let mut parts = Vec::new();
    if !filter.source.is_empty() {
        parts.push(format!(
            "Provider[@Name='{0}' or @EventSourceName='{0}']",
            filter.source
        ));
    }
    if !filter.event_ids.is_empty() {
        let ids = filter
            .event_ids
            .iter()
            .map(|id| format!("EventID={id}"))
            .collect::<Vec<_>>();
        parts.push(format!("({})", any_of(&ids)));
    }
    if !filter.levels.is_empty() {
        let levels = filter
            .levels
            .iter()
            .flat_map(|level| level.windows_levels())
            .map(|level| format!("Level={level}"))
            .collect::<Vec<_>>();
        parts.push(format!("({})", any_of(&levels)));
    }
    match records.after {
        Some(after) => parts.push(format!("EventRecordID > {after}")),
        None => parts.push(format!(
            "TimeCreated[timediff(@SystemTime) <= {}]",
            records.age_ms
        )),
    }
    parts.push(format!("EventRecordID <= {}", records.through));
    format!("*[System[{}]]", parts.join(" and "))
}

/// An entry of `journalctl --output=json`.
pub(crate) fn journal_event(line: &str) -> Option<LogEvent> {
    let entry: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = |name: &str| entry.get(name).and_then(serde_json::Value::as_str);
    let at_ms = text("__REALTIME_TIMESTAMP")?.parse::<i64>().ok()? / 1000;
    let level = text("PRIORITY")
        .and_then(|priority| priority.parse::<u8>().ok())
        .and_then(EventLevel::from_journal_priority);
    // A message that isn't UTF-8 is an array of its bytes.
    let message = match entry.get("MESSAGE") {
        Some(serde_json::Value::String(message)) => message.clone(),
        Some(serde_json::Value::Array(bytes)) => String::from_utf8_lossy(
            &bytes
                .iter()
                .filter_map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
                .collect::<Vec<_>>(),
        )
        .into_owned(),
        _ => String::new(),
    };
    let mut sources = Vec::new();
    // The identifier first: it names the program, where the unit may be a
    // session or other scope.
    for name in ["SYSLOG_IDENTIFIER", "_SYSTEMD_UNIT", "_COMM"] {
        if let Some(value) = text(name)
            && !sources.iter().any(|source| source == value)
        {
            sources.push(value.to_owned());
        }
    }
    Some(LogEvent {
        at_ms,
        level,
        sources,
        event_id: None,
        message,
    })
}

/// The highest (least severe) journald priority any of `levels` matches,
/// for `journalctl --priority`: 6 (info) when they all match.
pub(crate) fn journal_max_priority(levels: &[EventLevel]) -> u8 {
    if levels.is_empty() {
        return 6;
    }
    levels
        .iter()
        .flat_map(|level| level.journal_priorities().iter().copied())
        .max()
        .unwrap_or(6)
}

/// The levels as `log show --predicate` selects them; `None` matches every
/// type but debug.
pub(crate) fn unified_log_predicate(levels: &[EventLevel]) -> String {
    let types = if levels.is_empty() {
        vec!["default", "info", "error", "fault"]
    } else {
        levels
            .iter()
            .map(|level| match level {
                EventLevel::Critical => "fault",
                EventLevel::Error => "error",
                EventLevel::Warning => "default",
                EventLevel::Information => "info",
            })
            .collect()
    };
    types
        .iter()
        .map(|kind| format!("messageType == {kind}"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// An entry of `log show --style ndjson`, the macOS unified log. Its types
/// map onto levels: fault is critical, error is error, default is warning
/// and info is information.
pub(crate) fn unified_log_event(line: &str) -> Option<LogEvent> {
    let entry: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = |name: &str| entry.get(name).and_then(serde_json::Value::as_str);
    let at_ms = parse_timestamp(text("timestamp")?)?;
    let level = match text("messageType")? {
        "Fault" => Some(EventLevel::Critical),
        "Error" => Some(EventLevel::Error),
        "Default" => Some(EventLevel::Warning),
        "Info" => Some(EventLevel::Information),
        _ => None,
    };
    let mut sources = Vec::new();
    for value in [
        text("subsystem").unwrap_or_default(),
        file_name(text("processImagePath").unwrap_or_default()),
        file_name(text("senderImagePath").unwrap_or_default()),
    ] {
        if !value.is_empty() && !sources.iter().any(|source| source == value) {
            sources.push(value.to_owned());
        }
    }
    Some(LogEvent {
        at_ms,
        level,
        sources,
        event_id: None,
        message: text("eventMessage").unwrap_or_default().to_owned(),
    })
}

/// A service as its manager describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceStatus {
    pub(crate) found: bool,
    pub(crate) running: bool,
    /// Whether it starts by itself; `None` when the manager doesn't say.
    pub(crate) automatic: Option<bool>,
    /// Its state in the manager's words, such as `inactive (dead)`.
    pub(crate) state: String,
}

/// `systemctl show --property=LoadState,ActiveState,SubState,UnitFileState`.
/// A unit that is enabled, or that others start (static, generated or
/// indirect), starts by itself.
pub(crate) fn systemd_unit(text: &str) -> ServiceStatus {
    let value = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .unwrap_or_default()
            .trim()
    };
    let active = value("ActiveState");
    let sub = value("SubState");
    let file_state = value("UnitFileState");
    ServiceStatus {
        found: !matches!(value("LoadState"), "not-found" | ""),
        running: active == "active",
        automatic: (!file_state.is_empty()).then_some(matches!(
            file_state,
            "enabled" | "enabled-runtime" | "static" | "generated" | "indirect" | "alias"
        )),
        state: if sub.is_empty() {
            active.to_owned()
        } else {
            format!("{active} ({sub})")
        },
    }
}

/// `launchctl print system/<label>`: a job that is loaded, and running when
/// its state says so. Without `print`'s output the job isn't loaded.
pub(crate) fn launchd_job(print: Option<&str>) -> ServiceStatus {
    let Some(print) = print else {
        return ServiceStatus {
            found: false,
            running: false,
            automatic: None,
            state: "not loaded".to_owned(),
        };
    };
    let state = print
        .lines()
        .find_map(|line| line.trim().strip_prefix("state = "))
        .unwrap_or("unknown")
        .trim()
        .to_owned();
    ServiceStatus {
        found: true,
        running: state == "running",
        automatic: None,
        state,
    }
}

/// Whether `launchctl print-disabled system` lists `label` as disabled:
/// `"label" => disabled` (or `=> true` before macOS 13).
pub(crate) fn launchd_disabled(print_disabled: &str, label: &str) -> bool {
    let quoted = format!("\"{label}\"");
    print_disabled.lines().any(|line| {
        let line = line.trim();
        line.strip_prefix(&quoted)
            .and_then(|rest| rest.trim().strip_prefix("=>"))
            .is_some_and(|value| matches!(value.trim(), "disabled" | "true"))
    })
}

/// A process's name, accumulated CPU time and resident memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessSample {
    pub(crate) pid: u32,
    /// The names it can be matched by: its file name, and its command
    /// name where that differs.
    pub(crate) names: Vec<String>,
    pub(crate) cpu_ms: u64,
    pub(crate) memory_bytes: u64,
}

/// `[[dd-]hh:]mm:ss[.ss]`, as `ps -o time` writes CPU time, in milliseconds.
fn cpu_time(text: &str) -> Option<u64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut seconds = 0.0;
    for part in clock.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    // A time too long to count isn't one `ps` wrote.
    days.checked_mul(86_400_000)?
        .checked_add((seconds * 1000.0).round() as u64)
}

/// `ps -axo pid=,rss=,time=,comm=`: RSS in KiB, and the command, which on a
/// Mac is often its path.
pub(crate) fn ps_processes(text: &str) -> Vec<ProcessSample> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let rss = fields.next()?.parse::<u64>().ok()?;
            let cpu_ms = cpu_time(fields.next()?)?;
            let memory_bytes = rss.checked_mul(1024)?;
            let command = fields.collect::<Vec<_>>().join(" ");
            if command.is_empty() {
                return None;
            }
            let name = file_name(&command).to_owned();
            let mut names = vec![name.clone()];
            if name != command {
                names.push(command);
            }
            Some(ProcessSample {
                pid,
                names,
                cpu_ms,
                memory_bytes,
            })
        })
        .collect()
}

/// `/proc/<pid>/stat`: the PID, the command name (in parentheses, which it
/// may itself contain), user plus system CPU time in clock ticks, and
/// resident pages.
pub(crate) fn proc_stat(text: &str) -> Option<(u32, String, u64, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid = text.get(..open)?.trim().parse().ok()?;
    let comm = text.get(open + 1..close)?.to_owned();
    // Fields after the command, counting from state (field 3).
    let fields = text
        .get(close + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let field = |number: usize| fields.get(number - 3)?.parse::<u64>().ok();
    Some((pid, comm, field(14)?.checked_add(field(15)?)?, field(24)?))
}

/// Whether a process called `name` is the one a monitor names: ignoring
/// case and an `.exe` on Windows, exactly elsewhere.
pub(crate) fn process_name_matches(name: &str, wanted: &str, windows: bool) -> bool {
    if windows {
        let strip = |text: &str| {
            let lower = text.to_lowercase();
            lower
                .strip_suffix(".exe")
                .map(str::to_owned)
                .unwrap_or(lower)
        };
        strip(name) == strip(wanted)
    } else {
        name == wanted
    }
}

/// The round trip of a reply in `ping`'s output, in milliseconds:
/// `time=0.023 ms`, or `time<1 ms`.
pub(crate) fn ping_time_ms(output: &str) -> Option<f64> {
    output.lines().find_map(|line| {
        let at = line
            .find("time=")
            .map(|at| at + 5)
            .or_else(|| line.find("time<").map(|at| at + 5))?;
        let digits = line
            .get(at..)?
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>();
        digits.parse().ok()
    })
}

/// One DER item: its tag, contents and what follows.
fn der_item(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 {
            return None;
        }
        let (length, rest) = rest.split_at_checked(count)?;
        let length = length
            .iter()
            .fold(0usize, |length, byte| length << 8 | usize::from(*byte));
        (length, rest)
    };
    let (content, rest) = rest.split_at_checked(length)?;
    Some((tag, content, rest))
}

/// A UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`), in
/// Unix milliseconds. The server a monitor points at sends it, so anything
/// else, in any characters, is `None`.
fn der_time(tag: u8, content: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(content).ok()?.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 if text.len() == 12 => {
            let (year, rest) = text.split_at_checked(2)?;
            let year = number(year)?;
            (if year >= 50 { 1900 + year } else { 2000 + year }, rest)
        }
        0x18 if text.len() >= 14 => {
            let (year, rest) = text.split_at_checked(4)?;
            (number(year)?, rest)
        }
        _ => return None,
    };
    let field = |at: usize| number(rest.get(at..at + 2)?);
    let days = days_from_civil(year, field(0)?, field(2)?);
    Some((days * 86_400 + field(4)? * 3600 + field(6)? * 60 + field(8)?) * 1000)
}

/// When an X.509 certificate (DER) stops being valid, in Unix milliseconds.
pub(crate) fn certificate_not_after(der: &[u8]) -> Option<i64> {
    let (0x30, certificate, _) = der_item(der)? else {
        return None;
    };
    let (0x30, mut tbs, _) = der_item(certificate)? else {
        return None;
    };
    // An explicit version comes first in a v2 or v3 certificate.
    if tbs.first() == Some(&0xa0) {
        tbs = der_item(tbs)?.2;
    }
    // Serial number, signature algorithm, issuer, then validity.
    for _ in 0..3 {
        tbs = der_item(tbs)?.2;
    }
    let (0x30, validity, _) = der_item(tbs)? else {
        return None;
    };
    let (_, _, rest) = der_item(validity)?;
    let (tag, not_after, _) = der_item(rest)?;
    der_time(tag, not_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_convert_both_ways() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(format_time(1_794_232_317_849), "2026-11-09 13:51:57 UTC");
        assert_eq!(
            parse_timestamp("2026-11-09T09:38:10.5016551Z"),
            Some(1_794_217_090_501)
        );
        assert_eq!(
            parse_timestamp("2026-11-09 01:38:10.501655-0800"),
            Some(1_794_217_090_501)
        );
        assert_eq!(parse_timestamp("2026-11-09"), None);
    }

    #[test]
    fn windows_events_read_from_rendered_xml() {
        let events = windows_events(include_str!("testdata/windows-events.xml"));
        assert_eq!(events.len(), 3);
        let first = &events[0];
        assert_eq!(first.event_id, Some(7034));
        assert_eq!(first.level, Some(EventLevel::Error));
        assert_eq!(first.sources, ["Service Control Manager"]);
        assert_eq!(
            first.at_ms,
            parse_timestamp("2026-11-09T09:38:10.5016551Z").unwrap()
        );
        assert_eq!(
            first.message,
            "The Windows Update service terminated unexpectedly.  It has done this 77 time(s)."
        );
        let vss = &events[2];
        assert_eq!(vss.sources, ["VSS"]);
        assert_eq!(vss.event_id, Some(8224));
        assert_eq!(vss.level, Some(EventLevel::Information));
        assert_eq!(
            vss.message,
            "The VSS service is shutting down due to idle timeout."
        );
        // Without a rendered message, the event's data stands in.
        let xml = include_str!("testdata/windows-events.xml");
        let first_event = xml.split("</Event>").next().unwrap();
        let rendering = first_event.find("<RenderingInfo").unwrap();
        let bare = &first_event[..rendering];
        assert_eq!(
            windows_event(bare, None).unwrap().message,
            "Windows Update; 77"
        );
        assert_eq!(
            windows_event(bare, Some("From the publisher".into()))
                .unwrap()
                .message,
            "From the publisher"
        );

        let filter = EventFilter {
            source: "service control manager".into(),
            event_ids: vec![7031, 7034],
            levels: vec![EventLevel::Error, EventLevel::Critical],
            message: "WINDOWS UPDATE".into(),
        };
        assert_eq!(
            events.iter().filter(|event| filter.matches(event)).count(),
            1
        );
        let first = Records {
            after: None,
            through: 120,
            age_ms: 300_000,
        };
        assert_eq!(
            windows_query(&filter, first),
            "*[System[Provider[@Name='service control manager' or @EventSourceName='service control manager'] and (EventID=7031 or EventID=7034) and (Level=2 or Level=1) and TimeCreated[timediff(@SystemTime) <= 300000] and EventRecordID <= 120]]"
        );
        assert_eq!(
            windows_query(
                &EventFilter {
                    levels: vec![EventLevel::Information],
                    ..EventFilter::default()
                },
                Records {
                    after: Some(100),
                    ..first
                }
            ),
            "*[System[(Level=0 or Level=4) and EventRecordID > 100 and EventRecordID <= 120]]"
        );
        assert_eq!(windows_record(xml), Some(20_991));
        assert_eq!(
            windows_record("<System><EventRecordID>é</EventRecordID>"),
            None
        );
    }

    /// How deep a Windows Event Log query's expression nests, as the Event
    /// Log counts it: each `and` or `or` of a list one deeper than the one
    /// before it, and each group as deep as its own list.
    fn nesting(expression: &str) -> usize {
        let (mut operands, mut level, mut start) = (Vec::new(), 0i32, 0);
        let bytes = expression.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            match bytes[at] {
                b'(' | b'[' => level += 1,
                b')' | b']' => level -= 1,
                b' ' if level == 0 => {
                    let rest = &expression[at..];
                    if let Some(operator) = [" and ", " or "]
                        .into_iter()
                        .find(|operator| rest.starts_with(operator))
                    {
                        operands.push(&expression[start..at]);
                        at += operator.len();
                        start = at;
                        continue;
                    }
                }
                _ => {}
            }
            at += 1;
        }
        operands.push(&expression[start..]);
        let depth = |operand: &str| match operand
            .strip_prefix('(')
            .and_then(|inner| inner.strip_suffix(')'))
        {
            Some(inner) => nesting(inner),
            None => 1,
        };
        operands
            .iter()
            .skip(1)
            .fold(depth(operands[0]), |deepest, operand| {
                1 + deepest.max(depth(operand))
            })
    }

    /// The deepest query the Windows Event Log takes, measured on Windows 11
    /// against the Application log: 23 event IDs in a plain `or` list are
    /// taken, 24 aren't, nor are 23 and a time; source and level terms
    /// count only as one more step each.
    const WINDOWS_MAX_NESTING: usize = 23;

    fn system_expression(query: &str) -> &str {
        query
            .strip_prefix("*[System[")
            .and_then(|rest| rest.strip_suffix("]]"))
            .unwrap()
    }

    #[test]
    fn windows_queries_nest_within_what_the_event_log_takes() {
        // The measurements, as the count above models them.
        let plain = |count: u32| {
            (0..count)
                .map(|id| format!("EventID={id}"))
                .collect::<Vec<_>>()
                .join(" or ")
        };
        assert_eq!(nesting(&plain(23)), 23);
        assert_eq!(nesting(&plain(24)), 24);
        assert_eq!(nesting(&format!("({}) and TimeCreated[x]", plain(23))), 24);

        let mut every = EventFilter {
            source: "Application Error".into(),
            levels: EventLevel::ALL.to_vec(),
            message: "disk".into(),
            ..EventFilter::default()
        };
        for count in [1u32, 20, 21, 32] {
            every.event_ids = (0..count).map(|id| 1000 + id * 3).collect();
            for after in [None, Some(5)] {
                let records = Records {
                    after,
                    through: 9,
                    age_ms: 60_000,
                };
                let query = windows_query(&every, records);
                let expression = system_expression(&query);
                // Well within the limit, with room to spare.
                assert!(
                    nesting(expression) <= WINDOWS_MAX_NESTING - 10,
                    "{count} IDs nest {} deep: {query}",
                    nesting(expression)
                );
                // Exactly the IDs asked for, once each.
                let mut ids = query
                    .split("EventID=")
                    .skip(1)
                    .map(|rest| {
                        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                        rest[..digits].parse::<u32>().unwrap()
                    })
                    .collect::<Vec<_>>();
                ids.sort_unstable();
                assert_eq!(ids, every.event_ids);
                // And every other part of the filter.
                assert!(query.contains(
                    "Provider[@Name='Application Error' or @EventSourceName='Application Error']"
                ));
                for level in [0, 1, 2, 3, 4] {
                    assert_eq!(query.matches(&format!("Level={level}")).count(), 1);
                }
                assert!(query.contains("EventRecordID <= 9"));
                assert_eq!(query.contains("EventRecordID > 5"), after.is_some());
                assert_eq!(
                    query.contains("TimeCreated[timediff(@SystemTime) <= 60000]"),
                    after.is_none()
                );
            }
        }
        every.event_ids = vec![7];
        assert!(
            windows_query(
                &every,
                Records {
                    after: None,
                    through: 9,
                    age_ms: 1
                }
            )
            .contains(" and (EventID=7) and ")
        );
    }

    #[test]
    fn journal_entries_read_from_journalctl_json() {
        let events = include_str!("testdata/journal.json")
            .lines()
            .filter_map(journal_event)
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 4);
        let disk = events
            .iter()
            .find(|event| event.message.contains("Disk"))
            .unwrap();
        assert_eq!(disk.message, "Disk \"D:\" failed & retried");
        assert_eq!(disk.level, Some(EventLevel::Warning));
        assert!(
            disk.sources
                .iter()
                .any(|source| source == "meshrmm-fixture")
        );
        let user = &events[3];
        assert_eq!(user.sources[0], "systemd", "the identifier comes first");
        assert!(
            user.sources
                .iter()
                .any(|source| source == "user@1000.service")
        );
        let unit = EventFilter {
            source: "user@1000".into(),
            ..EventFilter::default()
        };
        assert!(unit.matches(user), "a unit matches without its .service");
        assert!(!unit.matches(disk));
        // An event shows the source its filter matched, else its first.
        assert_eq!(unit.source_of(user), Some("user@1000.service"));
        assert_eq!(EventFilter::default().source_of(user), Some("systemd"));
        let bytes = journal_event(
            r#"{"__REALTIME_TIMESTAMP":"1000000","PRIORITY":"2","MESSAGE":[104,105,255]}"#,
        )
        .unwrap();
        assert_eq!(bytes.message, "hi\u{fffd}");
        assert_eq!(bytes.level, Some(EventLevel::Critical));
        assert_eq!(bytes.at_ms, 1000);
        assert_eq!(
            journal_max_priority(&[EventLevel::Critical, EventLevel::Warning]),
            4
        );
        assert_eq!(journal_max_priority(&[]), 6);
    }

    #[test]
    fn unified_log_entries_read_from_ndjson() {
        let line = r#"{"traceID":2,"eventMessage":"Disk write failed: I/O error","eventType":"logEvent","source":null,"formatString":"%s","activityIdentifier":0,"subsystem":"com.apple.storage","category":"io","threadID":7,"senderImageUUID":"A","processImagePath":"\/usr\/libexec\/storagekitd","senderImagePath":"\/System\/Library\/PrivateFrameworks\/StorageKit.framework\/StorageKit","timestamp":"2026-11-09 01:38:10.501655-0800","machTimestamp":1,"messageType":"Error","processImageUUID":"B","processID":312,"senderProgramCounter":1,"parentActivityIdentifier":0,"timezoneName":""}"#;
        let event = unified_log_event(line).unwrap();
        assert_eq!(event.level, Some(EventLevel::Error));
        assert_eq!(
            event.sources,
            ["com.apple.storage", "storagekitd", "StorageKit"]
        );
        assert_eq!(event.at_ms, 1_794_217_090_501);
        assert!(
            unified_log_event("Filtering the log data using \"messageType == error\"").is_none()
        );
        assert_eq!(
            unified_log_predicate(&[EventLevel::Critical, EventLevel::Error]),
            "messageType == fault OR messageType == error"
        );
    }

    #[test]
    fn service_managers_say_whether_services_run() {
        let text = include_str!("testdata/systemctl-show.txt");
        let (cron, missing) = text.split_once("\n\n").unwrap();
        assert_eq!(
            systemd_unit(cron),
            ServiceStatus {
                found: true,
                running: true,
                automatic: Some(true),
                state: "active (running)".into()
            }
        );
        let missing = systemd_unit(missing);
        assert!(!missing.found && !missing.running);
        assert_eq!(missing.automatic, None);
        assert_eq!(
            systemd_unit(
                "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=disabled\n"
            )
            .automatic,
            Some(false)
        );

        let print = "system/com.example.backup = {\n\tactive count = 0\n\tpath = /Library/LaunchDaemons/com.example.backup.plist\n\ttype = LaunchDaemon\n\tstate = not running\n\n\tprogram = /usr/local/bin/backupd\n}\n";
        let job = launchd_job(Some(print));
        assert!(job.found && !job.running);
        assert_eq!(job.state, "not running");
        assert!(launchd_job(Some(&print.replace("not running", "running"))).running);
        assert!(!launchd_job(None).found);
        let disabled = "disabled services = {\n\t\"com.example.backup\" => disabled\n\t\"com.apple.ftpd\" => true\n\t\"com.example.web\" => enabled\n}\n";
        assert!(launchd_disabled(disabled, "com.example.backup"));
        assert!(launchd_disabled(disabled, "com.apple.ftpd"));
        assert!(!launchd_disabled(disabled, "com.example.web"));
        assert!(!launchd_disabled(disabled, "com.example"));
    }

    #[test]
    fn processes_read_from_ps_and_proc() {
        let ps = "      1 13144 00:00:00 systemd\n    312  2048 1-02:03:04 /usr/libexec/storagekitd\n    400  1024 0:01.50 /Applications/My App.app/Contents/MacOS/My App\n";
        let processes = ps_processes(ps);
        assert_eq!(processes.len(), 3);
        assert_eq!(processes[0].names, ["systemd"]);
        assert_eq!(processes[0].memory_bytes, 13_144 * 1024);
        assert_eq!(processes[1].cpu_ms, ((24 + 2) * 3600 + 3 * 60 + 4) * 1000);
        assert_eq!(
            processes[1].names,
            ["storagekitd", "/usr/libexec/storagekitd"]
        );
        assert_eq!(processes[2].names[0], "My App");
        assert_eq!(processes[2].cpu_ms, 1500);

        let mut lines = include_str!("testdata/proc-stat.txt").lines();
        assert_eq!(
            proc_stat(lines.next().unwrap()),
            Some((1, "systemd".into(), 19 + 17, 3073))
        );
        assert_eq!(
            proc_stat("77 (my (odd) daemon) S 1 77 77 0 -1 0 0 0 0 0 5 6 0 0 20 0 1 0 1 1 9"),
            Some((77, "my (odd) daemon".into(), 11, 9))
        );
        assert!(process_name_matches("SQLSERVR.EXE", "sqlservr", true));
        assert!(process_name_matches("sqlservr.exe", "SqlServr.exe", true));
        assert!(!process_name_matches("Sqlservr", "sqlservr", false));
    }

    #[test]
    fn ping_reports_its_round_trip() {
        assert_eq!(
            ping_time_ms(include_str!("testdata/linux-ping-reply.txt")),
            Some(0.020)
        );
        assert_eq!(
            ping_time_ms("Reply from 127.0.0.1: bytes=32 time<1ms TTL=128"),
            Some(1.0)
        );
        assert_eq!(
            ping_time_ms("1 packets transmitted, 0 received, 100% packet loss, time 0ms"),
            None
        );
    }

    #[test]
    fn certificates_say_when_they_expire() {
        let utc = include_bytes!("testdata/certificate-utctime.der");
        assert_eq!(
            certificate_not_after(utc).map(format_date).as_deref(),
            Some(include_str!("testdata/certificate-utctime.expires").trim())
        );
        let generalized = include_bytes!("testdata/certificate-generalized.der");
        assert_eq!(
            certificate_not_after(generalized)
                .map(format_date)
                .as_deref(),
            Some(include_str!("testdata/certificate-generalized.expires").trim())
        );
        assert_eq!(certificate_not_after(&utc[..20]), None);
        assert_eq!(certificate_not_after(b""), None);
    }

    /// Where a certificate's expiry is: the content of the first time of
    /// that type and length that reads as it.
    fn not_after_at(der: &[u8], tag: u8, length: u8) -> usize {
        let expires = certificate_not_after(der);
        assert!(expires.is_some());
        der.windows(2)
            .enumerate()
            .filter(|(_, pair)| *pair == [tag, length])
            .map(|(at, _)| at + 2)
            .find(|at| {
                der.get(*at..*at + usize::from(length))
                    .and_then(|time| der_time(tag, time))
                    == expires
            })
            .unwrap()
    }

    /// A server can present a certificate with anything in its times, and
    /// the check then fails to read the expiry instead of panicking.
    #[test]
    fn certificates_with_unreadable_times_have_no_expiry() {
        let e = "é".as_bytes();
        for (der, tag, length) in [
            (
                include_bytes!("testdata/certificate-utctime.der").as_slice(),
                0x17,
                13,
            ),
            (
                include_bytes!("testdata/certificate-generalized.der").as_slice(),
                0x18,
                15,
            ),
        ] {
            let at = not_after_at(der, tag, length);
            // A two-byte character across where the year ends, and inside
            // each later field.
            for offset in 1..usize::from(length) - 2 {
                let mut forged = der.to_vec();
                forged[at + offset..at + offset + 2].copy_from_slice(e);
                assert_eq!(certificate_not_after(&forged), None, "at {offset}");
            }
            // Cut short anywhere.
            for end in 0..der.len() {
                let _ = certificate_not_after(&der[..end]);
            }
        }
        for (tag, text) in [
            (0x17, "2é123123595Z"),
            (0x17, "26é12312359Z"),
            (0x17, "é"),
            (0x18, "202é123123595Z"),
            (0x18, "2026é12312359Z"),
            (0x17, "2612Z"),
            (0x18, "2026Z"),
            (0x17, "Z"),
            (0x17, ""),
            (0x18, ""),
        ] {
            assert_eq!(der_time(tag, text.as_bytes()), None, "{text}");
        }
        assert_eq!(der_time(0x17, &[0xff, b'Z']), None, "not UTF-8");
    }

    #[test]
    fn unreadable_log_times_have_none() {
        for text in [
            // A two-byte character across where the date ends, where the
            // clock ends, and in the zone.
            "2026-11-0é09:38:10Z",
            "2026-11-09T08:10:1é",
            "2026-11-09T08:10:10+é12",
            "2026-11-09T08:10:10.5é",
            "éééééééééééééééééééé",
            "2026-11-09T08:10",
            "2026-11-09T08:10:10+08",
            "",
        ] {
            assert_eq!(parse_timestamp(text), None, "{text}");
        }
        let line = r#"{"timestamp":"2026-11-0é01:38:10.501655-0800","messageType":"Error"}"#;
        assert_eq!(unified_log_event(line), None);
    }

    #[test]
    fn unreadable_windows_events_have_none() {
        let xml = include_str!("testdata/windows-events.xml");
        let first = xml.split("</Event>").next().unwrap();
        // Cut short anywhere, or with a two-byte character anywhere.
        for (at, _) in first.char_indices() {
            let _ = windows_event(&first[..at], None);
            let _ = windows_event(&format!("{}é{}", &first[..at], &first[at..]), None);
        }
        assert_eq!(
            windows_provider(first).as_deref(),
            Some("Service Control Manager")
        );
        assert_eq!(
            windows_provider("<Provider EventSourceName='x' Name=\"a &amp; b\"/>").as_deref(),
            Some("a & b")
        );
        for xml in [
            "<Provider Name=éx'/>",
            "<Provider Name='abc",
            "<Provider Name=",
            "<Provideré",
            "<Provider",
            "",
        ] {
            assert_eq!(windows_provider(xml), None, "{xml}");
        }
        assert_eq!(unescape("a &#xé; &é; &amp"), "a &#xé; &é; &amp");
        assert_eq!(unescape("&lt;é&gt;"), "<é>");
    }

    #[test]
    fn unreadable_tool_output_is_skipped() {
        let max = u64::MAX;
        // Memory or CPU time too large to count.
        assert!(ps_processes(&format!("1 {max} 00:00:01 a\n")).is_empty());
        assert!(ps_processes(&format!("1 1 {max}-00:00:01 a\n")).is_empty());
        assert_eq!(cpu_time("213503982334601-00:00:00"), None);
        assert_eq!(ps_processes("1 1 é\n2 2 00:00:01 é\n")[0].names, ["é"]);
        assert_eq!(
            proc_stat(&format!(
                "77 (d) S 1 77 77 0 -1 0 0 0 0 0 {max} {max} 0 0 20 0 1 0 1 1 9"
            )),
            None
        );
        assert_eq!(proc_stat("1 ) (x S"), None);
        assert_eq!(proc_stat("é (é) S"), None);
        assert_eq!(proc_stat(""), None);
        assert_eq!(ping_time_ms("time=é ms\ntime<"), None);
    }
}
