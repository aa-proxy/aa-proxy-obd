// src/logging.rs
//
// Console + file logging. Level comes from [daemon].log_level; --debug forces
// Debug. If the log file can't be opened, fall back to console-only.

use simplelog::*;
use std::fs::OpenOptions;
use std::str::FromStr;

pub fn init(log_level: &str, log_file: &str, debug_override: bool) {
    let level = if debug_override {
        LevelFilter::Debug
    } else {
        LevelFilter::from_str(log_level).unwrap_or(LevelFilter::Info)
    };

    let conf = ConfigBuilder::new()
        .set_time_format_rfc3339()
        .build();

    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![
        TermLogger::new(level, conf.clone(), TerminalMode::Mixed, ColorChoice::Auto),
    ];

    let mut file_open_err: Option<String> = None;
    match OpenOptions::new().create(true).append(true).open(log_file) {
        Ok(f) => loggers.push(WriteLogger::new(level, conf, f)),
        Err(e) => file_open_err = Some(format!("cannot open log file '{}': {}", log_file, e)),
    }

    CombinedLogger::init(loggers).expect("logger init");
    if let Some(msg) = file_open_err {
        log::warn!("{msg}; continuing with console-only logging");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_from_str_parses_known_levels() {
        assert_eq!(LevelFilter::from_str("trace").unwrap(), LevelFilter::Trace);
        assert_eq!(LevelFilter::from_str("debug").unwrap(), LevelFilter::Debug);
        assert_eq!(LevelFilter::from_str("info").unwrap(),  LevelFilter::Info);
        assert_eq!(LevelFilter::from_str("warn").unwrap(),  LevelFilter::Warn);
        assert_eq!(LevelFilter::from_str("error").unwrap(), LevelFilter::Error);
        assert_eq!(LevelFilter::from_str("off").unwrap(),   LevelFilter::Off);
    }
}
