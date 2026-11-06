use std::{
    io::Write,
    sync::{
        Mutex, Once,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use crate::clock::utc_timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

static MIN_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn init_from_env() {
    let level = match std::env::var("LOG_LEVEL").unwrap_or_default().to_ascii_lowercase().as_str() {
        "debug" => Level::Debug,
        "warn" | "warning" => Level::Warn,
        "error" => Level::Error,
        _ => Level::Info,
    };
    set_min_level(level);
}

pub fn set_min_level(level: Level) {
    MIN_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn write(level: Level, target: &str, message: &str) {
    if (level as u8) < MIN_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let tag = match level {
        Level::Debug => "DEBUG",
        Level::Info => "INFO ",
        Level::Warn => "WARN ",
        Level::Error => "ERROR",
    };
    eprintln!("{} {tag} [{target}] {message}", utc_timestamp());
}

const ACCESS_QUEUE_LIMIT: usize = 8 << 20;
static ACCESS_QUEUE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static ACCESS_WRITER: Once = Once::new();

pub fn access(line: String) {
    ACCESS_WRITER.call_once(|| {
        std::thread::Builder::new()
            .name("access-log".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_millis(200));
                flush();
            })
            .expect("spawning the access-log thread");
    });
    let stamp = utc_timestamp();
    let mut queue = ACCESS_QUEUE.lock().unwrap();
    if queue.len() < ACCESS_QUEUE_LIMIT {
        queue.extend_from_slice(stamp.as_bytes());
        queue.extend_from_slice(b" ACCESS ");
        queue.extend_from_slice(line.as_bytes());
        queue.push(b'\n');
    }
}

pub fn flush() {
    let queued = std::mem::take(&mut *ACCESS_QUEUE.lock().unwrap());
    if !queued.is_empty() {
        let _ = std::io::stderr().write_all(&queued);
    }
}

macro_rules! log_debug { ($target:expr, $($fmt:tt)*) => { $crate::log::write($crate::log::Level::Debug, $target, &format!($($fmt)*)) } }
macro_rules! log_info { ($target:expr, $($fmt:tt)*) => { $crate::log::write($crate::log::Level::Info, $target, &format!($($fmt)*)) } }
macro_rules! log_warn { ($target:expr, $($fmt:tt)*) => { $crate::log::write($crate::log::Level::Warn, $target, &format!($($fmt)*)) } }
macro_rules! log_error { ($target:expr, $($fmt:tt)*) => { $crate::log::write($crate::log::Level::Error, $target, &format!($($fmt)*)) } }
pub(crate) use {log_debug, log_error, log_info, log_warn};
