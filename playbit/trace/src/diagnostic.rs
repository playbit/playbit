//! Bounded native diagnostic records with UTC time and process identity.
use std::{
    fmt::{self, Write as _},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

pub const REPORT_BYTES: usize = 64 * 1024;
const MAX_PROCESS_NAME: usize = 64;

pub struct Report {
    bytes: Box<[u8]>,
    length: usize,
    prefix: String,
    line_start: bool,
}

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

impl Report {
    pub fn new() -> Self {
        static PROCESS: OnceLock<String> = OnceLock::new();
        let process = PROCESS.get_or_init(|| {
            std::env::current_exe()
                .expect("running executable has a path")
                .file_name()
                .expect("running executable has a name")
                .to_string_lossy()
                .chars()
                .take(MAX_PROCESS_NAME)
                .collect()
        });
        Self {
            bytes: vec![0; REPORT_BYTES].into_boxed_slice(),
            length: 0,
            prefix: prefix(SystemTime::now(), process, std::process::id()),
            line_start: true,
        }
    }

    pub fn text(&mut self) -> &str {
        if self.length > 0 && self.bytes[self.length - 1] != b'\n' {
            self.bytes[self.length] = b'\n';
            self.length += 1;
            self.line_start = true;
        }
        std::str::from_utf8(&self.bytes[..self.length])
            .expect("report copies complete UTF-8 prefixes")
    }

    fn append(&mut self, text: &str) -> fmt::Result {
        // Reserve a final newline so truncated records cannot join the next record.
        let mut length = text.len().min((REPORT_BYTES - 1).saturating_sub(self.length));
        while !text.is_char_boundary(length) {
            length -= 1;
        }
        self.bytes[self.length..self.length + length].copy_from_slice(&text.as_bytes()[..length]);
        self.length += length;
        if length < text.len() { Err(fmt::Error) } else { Ok(()) }
    }
}

impl fmt::Write for Report {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for line in text.split_inclusive('\n') {
            if self.line_start {
                if REPORT_BYTES.saturating_sub(self.length) <= self.prefix.len() + 1 {
                    return Err(fmt::Error);
                }
                let end = self.length + self.prefix.len();
                self.bytes[self.length..end].copy_from_slice(self.prefix.as_bytes());
                self.length = end;
            }
            self.line_start = line.ends_with('\n');
            self.append(line)?;
        }
        Ok(())
    }
}

#[allow(unsafe_code, reason = "gmtime_r writes one owned tm from a valid time_t pointer.")]
fn prefix(now: SystemTime, process: &str, pid: u32) -> String {
    let elapsed = now.duration_since(UNIX_EPOCH).expect("system clock is after the Unix epoch");
    let seconds = elapsed.as_secs().try_into().expect("system time fits time_t");
    let mut utc = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers remain valid throughout the call; success initializes tm.
    let utc = unsafe {
        assert!(!libc::gmtime_r(&seconds, utc.as_mut_ptr()).is_null(), "system time fits UTC");
        utc.assume_init()
    };
    format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {process} {pid}] ",
        utc.tm_year + 1900,
        utc.tm_mon + 1,
        utc.tm_mday,
        utc.tm_hour,
        utc.tm_min,
        utc.tm_sec,
        elapsed.subsec_millis(),
    )
}

pub fn stderr(arguments: fmt::Arguments<'_>) {
    use std::io::Write as _;
    let mut report = Report::new();
    let _ = writeln!(report, "{arguments}");
    let _ = std::io::stderr().lock().write_all(report.text().as_bytes());
}

/// Display one log record with a single prefix, including multiline backtraces.
pub fn stderr_record(message: &str) {
    use std::io::Write as _;
    let mut report = Report::new();
    let (first, rest) = message.split_once('\n').unwrap_or((message, ""));
    if writeln!(report, "{first}").is_ok() {
        let _ = report.append(rest);
    }
    let _ = std::io::stderr().lock().write_all(report.text().as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_prefix_includes_milliseconds_process_and_pid() {
        // A fixed instant verifies UTC calendar conversion independently of the local timezone.
        let now = UNIX_EPOCH + std::time::Duration::from_millis(1_792_023_621_252);
        assert_eq!(
            prefix(now, "playbit-engine", 123),
            "[2026-10-15 00:20:21.252 playbit-engine 123] "
        );
        assert_eq!(prefix(UNIX_EPOCH, "pb", 1), "[1970-01-01 00:00:00.000 pb 1] ");
    }

    #[test]
    fn multiline_records_prefix_every_line_and_end_at_the_utf8_bound() {
        // Fragmented format writes and truncation retain a prefix on each physical line.
        std::thread::Builder::new()
            .stack_size(64 * 1024) // The trace-registration worker's existing stack.
            .spawn(|| {
                let mut report = Report::new();
                let prefix = report.prefix.clone();
                write!(report, "first {}\nsecond\n", 1).expect("short diagnostic");
                assert_eq!(report.text(), format!("{prefix}first 1\n{prefix}second\n"));
                let frames = format!("{}last guest frame", "guest::frame é\n".repeat(512));
                write!(report, "{frames}").expect("guest backtrace fits 64 KiB");
                let text = report.text();
                assert!(text.len() > 8192);
                assert!(text.ends_with(&format!("{prefix}last guest frame\n")));
                assert!(write!(report, "{}", "é".repeat(REPORT_BYTES)).is_err());
                let text = report.text();
                assert!(text.len() <= REPORT_BYTES);
                assert!(text.ends_with('\n'));
                assert!(text.lines().all(|line| line.starts_with(&prefix)));
            })
            .expect("diagnostic worker")
            .join()
            .expect("diagnostic formatting fits the trace-registration stack");
    }
}
