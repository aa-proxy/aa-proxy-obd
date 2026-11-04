// Console + file logging.
//
// Messages may contain paris-style markup (e.g. "<b><blue>text</>"). The
// console renders that markup to ANSI colour; the file receives the same text
// with the markup removed. Only recognised colour/style/reset tags are touched
// — any other angle-bracket token (e.g. a Rust type like `Vec<u8>` in an error
// message) is left exactly as written, so messages are never mangled.

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

/// True only for the paris colour/style/reset tags used as markup. Icons are
/// deliberately excluded so a word like "<info>" in a message is not treated as
/// markup.
fn is_markup_tag(inner: &str) -> bool {
    const STYLES: &[&str] = &[
        "bold", "b", "dimmed", "d", "italic", "i", "underline", "u",
        "blink", "l", "reverse", "r", "hidden", "h", "strikethrough", "s",
    ];
    const COLOURS: &[&str] = &[
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    if matches!(inner, "/" | "//" | "///") {
        return true; // foreground/background/all resets
    }
    if STYLES.contains(&inner) {
        return true;
    }
    // foreground colour, or "on <colour>" background, each optionally "bright".
    let body = inner.strip_prefix("on ").unwrap_or(inner);
    let body = body.strip_prefix("bright ").unwrap_or(body);
    COLOURS.contains(&body)
}

/// Render recognised paris markup tags in `line` (to ANSI when `colorize`,
/// stripped otherwise) while passing every other `<...>` token through
/// untouched.
fn render_markup(line: &str, colorize: bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open..]; // starts with '<'
        match after[1..].find(['>', '<']) {
            // A balanced "<...>" with no nested '<'.
            Some(rel) if after.as_bytes()[1 + rel] == b'>' => {
                let token_end = 1 + rel + 1; // include the closing '>'
                let token = &after[..token_end];
                let inner = &after[1..1 + rel];
                if is_markup_tag(inner) {
                    out.push_str(&paris::formatter::format_string(token, colorize));
                } else {
                    out.push_str(token); // not markup — keep verbatim
                }
                rest = &after[token_end..];
            }
            // No closing '>' before the next '<' (or end): the '<' is literal.
            _ => {
                out.push('<');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

const RESET: &str = "\x1b[0m";

/// Console form of a line: markup rendered, and with colour on, ended with a
/// single ANSI reset so an unclosed tag cannot colour the lines that follow.
fn render_console_line(line: &str, colorize: bool) -> String {
    let mut out = render_markup(line, colorize);
    if colorize && !out.ends_with(RESET) {
        out.push_str(RESET);
    }
    out
}

const FALLBACK_LEVEL: LevelFilter = LevelFilter::Info;

/// Resolves the configured level. The second value is a warning to emit once
/// the logger is installed when `log_level` could not be parsed.
fn resolve_level(log_level: &str, debug_override: bool) -> (LevelFilter, Option<String>) {
    match LevelFilter::from_str(log_level) {
        Ok(l) => (if debug_override { LevelFilter::Debug } else { l }, None),
        Err(_) => (
            if debug_override { LevelFilter::Debug } else { FALLBACK_LEVEL },
            Some(format!(
                "invalid log_level '{log_level}'; using '{}' (valid: off, error, warn, info, debug, trace)",
                if debug_override { "debug".to_string() } else { FALLBACK_LEVEL.to_string().to_lowercase() }
            )),
        ),
    }
}

pub fn init(log_level: &str, log_file: &str, debug_override: bool) -> Result<(), log::SetLoggerError> {
    let (level, level_warning) = resolve_level(log_level, debug_override);

    // Console: render markup to colour on an interactive terminal; if stdout is
    // redirected (piped, captured by a service manager), strip it to plain text.
    let colorize = std::io::stdout().is_terminal();
    let console = fern::Dispatch::new()
        .format(move |out, message, record| {
            let line = format!("{} [{}] {message}", timestamp(), level_markup(record.level()));
            out.finish(format_args!("{}", render_console_line(&line, colorize)))
        })
        .chain(std::io::stdout());

    let dispatch = fern::Dispatch::new().level(level).chain(console);

    // File: strip markup so the file holds plain text only.
    let (dispatch, file_err) = match OpenOptions::new().create(true).append(true).open(log_file) {
        Ok(file) => {
            let plain = fern::Dispatch::new()
                .format(|out, message, record| {
                    let line = format!("{} [{}] {message}", timestamp(), record.level());
                    out.finish(format_args!("{}", render_markup(&line, false)))
                })
                .chain(file);
            (dispatch.chain(plain), None)
        }
        Err(e) => (dispatch, Some(format!("cannot open log file '{}': {}", log_file, e))),
    };

    dispatch.apply()?;
    if let Some(msg) = level_warning {
        log::warn!("{msg}");
    }
    if let Some(msg) = file_err {
        log::warn!("{msg}; continuing with console-only logging");
    }
    Ok(())
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
    fn invalid_level_yields_a_warning_naming_value_and_fallback() {
        let (level, warning) = resolve_level("verbose", false);
        assert_eq!(level, LevelFilter::Info);
        let w = warning.expect("warning");
        assert!(w.contains("'verbose'") && w.contains("'info'"), "{w}");
    }

    #[test]
    fn invalid_level_still_warns_with_debug_override() {
        let (level, warning) = resolve_level("bogus", true);
        assert_eq!(level, LevelFilter::Debug);
        let w = warning.expect("warning");
        assert!(w.contains("'bogus'") && w.contains("'debug'"), "{w}");
    }

    #[test]
    fn second_init_returns_err() {
        let _ = init("info", "/dev/null", false);
        assert!(init("info", "/dev/null", false).is_err());
    }

    #[test]
    fn markup_renders_for_console_and_strips_for_file() {
        let msg = "<b><blue>aa-proxy-obd</> started";
        let colored = render_markup(msg, true);
        let plain = render_markup(msg, false);
        assert!(colored.contains('\u{1b}'), "console output should contain ANSI codes");
        assert!(!plain.contains('\u{1b}'), "file output must not contain ANSI codes");
        assert_eq!(plain, "aa-proxy-obd started");
    }

    #[test]
    fn non_markup_angle_brackets_are_preserved() {
        // Tokens that are not recognised tags (e.g. Rust type names) must
        // survive verbatim in both console and file output.
        for colorize in [true, false] {
            assert_eq!(
                render_markup("decoded Vec<u8> and Option<Stream>", colorize),
                "decoded Vec<u8> and Option<Stream>",
            );
        }
    }

    #[test]
    fn lone_or_spaced_angle_brackets_are_literal() {
        for colorize in [true, false] {
            assert_eq!(render_markup("a < b and c > d", colorize), "a < b and c > d");
            assert_eq!(render_markup("ends with <", colorize), "ends with <");
            assert_eq!(render_markup("<< double", colorize), "<< double");
        }
    }

    #[test]
    fn console_line_with_unclosed_tag_ends_with_reset_only_when_colour_is_on() {
        let on = render_console_line("<red>never closed", true);
        assert!(on.ends_with("\x1b[0m"), "{on:?}");
        let off = render_console_line("<red>never closed", false);
        assert_eq!(off, "never closed");
        assert!(!off.contains('\x1b'));
    }

    #[test]
    fn console_line_with_closed_tag_has_a_single_trailing_reset() {
        let on = render_console_line("<red>closed</>", true);
        assert!(on.ends_with(RESET), "{on:?}");
        assert!(!on.ends_with("\x1b[0m\x1b[0m"), "{on:?}");
        assert_eq!(on.matches(RESET).count(), 1, "{on:?}");
    }

    #[test]
    fn level_tag_strips_cleanly_in_file() {
        let line = format!("[{}] hello", level_markup(Level::Warn));
        assert_eq!(render_markup(&line, false), "[WARN] hello");
    }
}
