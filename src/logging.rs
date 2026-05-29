// Console + file logging.
//
// Messages may contain paris-style markup (e.g. "<b><blue>text</>"). The
// console renders that markup to ANSI colour; the file receives the same text
// with the markup removed, so log files stay free of escape codes.

use log::{Level, LevelFilter};
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::str::FromStr;

/// Level tag wrapped in paris colour markup. Rendered to colour on the
/// console and stripped to plain "[LEVEL]" in the file.
fn level_markup(level: Level) -> &'static str {
    match level {
        Level::Error => "<red>ERROR</>",
        Level::Warn => "<yellow>WARN</>",
        Level::Info => "<green>INFO</>",
        Level::Debug => "<cyan>DEBUG</>",
        Level::Trace => "<bright black>TRACE</>",
    }
}

fn timestamp() -> String {
    chrono::Local::now().format("%F, %H:%M:%S%.3f").to_string()
}

pub fn init(log_level: &str, log_file: &str, debug_override: bool) {
    let level = if debug_override {
        LevelFilter::Debug
    } else {
        LevelFilter::from_str(log_level).unwrap_or(LevelFilter::Info)
    };

    // Console: render markup to colour on an interactive terminal; if stdout is
    // redirected (piped, captured by a service manager), strip it to plain text.
    let colorize = std::io::stdout().is_terminal();
    let console = fern::Dispatch::new()
        .format(move |out, message, record| {
            let line = format!("{} [{}] {message}", timestamp(), level_markup(record.level()));
            let rendered = paris::formatter::format_string(line, colorize);
            out.finish(format_args!("{rendered}"))
        })
        .chain(std::io::stdout());

    let dispatch = fern::Dispatch::new().level(level).chain(console);

    // File: strip markup so the file holds plain text only.
    let (dispatch, file_err) = match OpenOptions::new().create(true).append(true).open(log_file) {
        Ok(file) => {
            let plain = fern::Dispatch::new()
                .format(|out, message, record| {
                    let line = format!("{} [{}] {message}", timestamp(), record.level());
                    let stripped = paris::formatter::format_string(line, false);
                    out.finish(format_args!("{stripped}"))
                })
                .chain(file);
            (dispatch.chain(plain), None)
        }
        Err(e) => (dispatch, Some(format!("cannot open log file '{}': {}", log_file, e))),
    };

    dispatch.apply().expect("logger init");
    if let Some(msg) = file_err {
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
        assert_eq!(LevelFilter::from_str("info").unwrap(), LevelFilter::Info);
        assert_eq!(LevelFilter::from_str("warn").unwrap(), LevelFilter::Warn);
        assert_eq!(LevelFilter::from_str("error").unwrap(), LevelFilter::Error);
        assert_eq!(LevelFilter::from_str("off").unwrap(), LevelFilter::Off);
    }

    #[test]
    fn markup_renders_for_console_and_strips_for_file() {
        let msg = "<b><blue>aa-proxy-obd</> started";
        let colored = paris::formatter::format_string(msg, true);
        let plain = paris::formatter::format_string(msg, false);
        // Console output carries ANSI escape codes; file output does not, and
        // neither carries the literal markup tags.
        assert!(colored.contains('\u{1b}'), "console output should contain ANSI codes");
        assert!(!plain.contains('\u{1b}'), "file output must not contain ANSI codes");
        assert!(!plain.contains('<'), "file output must not contain markup tags");
        assert_eq!(plain, "aa-proxy-obd started");
    }
}
