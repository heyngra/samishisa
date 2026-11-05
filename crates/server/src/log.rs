use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

static MIN: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn init_from_env() {
    let level = match std::env::var("LOG_LEVEL").unwrap_or_default().to_ascii_lowercase().as_str() {
        "debug" => Level::Debug,
        "warn" | "warning" => Level::Warn,
        "error" => Level::Error,
        _ => Level::Info,
    };
    MIN.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    level as u8 >= MIN.load(Ordering::Relaxed)
}

pub fn write(level: Level, target: &str, msg: &str) {
    if !enabled(level) {
        return;
    }
    let tag = match level {
        Level::Debug => "DEBUG",
        Level::Info => "INFO ",
        Level::Warn => "WARN ",
        Level::Error => "ERROR",
    };
    eprintln!("{} {tag} [{target}] {msg}", utc_now());
}

fn utc_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

macro_rules! log_info { ($t:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Info, $t, &format!($($a)*)) } }
macro_rules! log_warn { ($t:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Warn, $t, &format!($($a)*)) } }
macro_rules! log_error { ($t:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Error, $t, &format!($($a)*)) } }
pub(crate) use {log_error, log_info, log_warn};
