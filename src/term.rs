//! Tiny ANSI styling, dependency-free. Honors `NO_COLOR` and disables itself
//! when stdout is not a TTY (so piped/JSON output stays clean).

use std::io::IsTerminal;
use std::sync::OnceLock;

fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

fn wrap(code: &str, s: &str) -> String {
    if enabled() {
        format!("\x1b[{}m{}\x1b[0m", code, s)
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    wrap("1", s)
}
pub fn dim(s: &str) -> String {
    wrap("2", s)
}
pub fn red(s: &str) -> String {
    wrap("31", s)
}
pub fn green(s: &str) -> String {
    wrap("32", s)
}
pub fn yellow(s: &str) -> String {
    wrap("33", s)
}
pub fn blue(s: &str) -> String {
    wrap("34", s)
}
pub fn magenta(s: &str) -> String {
    wrap("35", s)
}
pub fn cyan(s: &str) -> String {
    wrap("36", s)
}
pub fn bold_red(s: &str) -> String {
    wrap("1;31", s)
}
pub fn bold_green(s: &str) -> String {
    wrap("1;32", s)
}
pub fn bold_yellow(s: &str) -> String {
    wrap("1;33", s)
}
pub fn bold_cyan(s: &str) -> String {
    wrap("1;36", s)
}
