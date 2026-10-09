//! Forwarding the helpers' stderr to the Agent log.
use super::*;

/// Forwards a helper's stderr to the Agent log. Some helpers run with the
/// user's token, so lines are capped in length and rate and the rest is
/// drained without being kept, to keep the pipe from blocking the helper.
pub(super) fn drain_child_stderr(stderr: File) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    let mut budget = LineBudget::new(STDERR_LINES_PER_WINDOW, STDERR_WINDOW, Instant::now());
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_STDERR_LINE_BYTES) {
            Ok(Some(truncated)) => {
                let (admitted, suppressed) = budget.admit(Instant::now());
                if suppressed > 0 {
                    tracing::warn!(suppressed, "suppressed desktop-helper stderr lines");
                }
                if admitted {
                    let message = String::from_utf8_lossy(&line);
                    tracing::warn!(%message, truncated, "desktop helper wrote to stderr");
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "failed to read desktop-helper stderr");
                break;
            }
        }
    }
    let suppressed = budget.take_suppressed();
    if suppressed > 0 {
        tracing::warn!(suppressed, "suppressed desktop-helper stderr lines");
    }
}

/// Reads one line into `line`, keeping at most `limit` bytes and discarding
/// the rest up to the newline. Returns whether the line was cut short, or
/// `None` at end of input.
pub(super) fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    limit: usize,
) -> io::Result<Option<bool>> {
    line.clear();
    let mut truncated = false;
    let mut read_any = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(read_any.then_some(truncated));
        }
        read_any = true;
        let newline = available.iter().position(|&byte| byte == b'\n');
        let content = &available[..newline.unwrap_or(available.len())];
        let room = limit.saturating_sub(line.len());
        truncated |= content.len() > room;
        line.extend_from_slice(&content[..content.len().min(room)]);
        let consumed = newline.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(truncated));
        }
    }
}

/// Allows `limit` lines per window and counts the lines it refuses.
pub(super) struct LineBudget {
    limit: u32,
    window: Duration,
    window_start: Instant,
    used: u32,
    suppressed: u64,
}

impl LineBudget {
    pub(super) fn new(limit: u32, window: Duration, now: Instant) -> Self {
        Self {
            limit,
            window,
            window_start: now,
            used: 0,
            suppressed: 0,
        }
    }

    /// Returns whether to log this line, and how many lines the window that
    /// just ended suppressed.
    pub(super) fn admit(&mut self, now: Instant) -> (bool, u64) {
        let mut ended = 0;
        if now.duration_since(self.window_start) >= self.window {
            ended = self.take_suppressed();
            self.window_start = now;
            self.used = 0;
        }
        if self.used < self.limit {
            self.used += 1;
            (true, ended)
        } else {
            self.suppressed += 1;
            (false, ended)
        }
    }

    pub(super) fn take_suppressed(&mut self) -> u64 {
        std::mem::take(&mut self.suppressed)
    }
}
