//! Shell-aware analysis of the command line passed to `launch_app`.
//!
//! `launch_app` writes one line to the container entrypoint, which runs it with
//! `eval`. That line is a full shell command, not a bare argv: agents routinely
//! send `google-chrome-stable https://example.com && echo launched`. Chromium
//! switches therefore cannot be appended to the end of the line — in that
//! example the switches would become arguments of `echo`, and Chrome would
//! start with no `--ozone-platform`, no `--password-store`, and no CDP port.
//! Substring scanning of the whole line is wrong in the other direction too: a
//! URL or an unrelated later command that merely contains `--password-store`
//! suppressed the injection Chrome actually needed, and any command whose text
//! contained `code` was mistaken for VS Code.
//!
//! This module locates the browser's own command inside the line and reports
//! where that command's argv starts, so switches are inserted directly after
//! the browser program word. Nothing else in the line is rewritten: the caller
//! splices the switches into the original text at a byte offset this module
//! computed from a quote-, escape-, and substitution-aware scan.

/// A launch command that cannot be split into shell words.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CommandParseError {
    #[error("unterminated {quote} quote in launch command")]
    UnterminatedQuote { quote: char },
    #[error("unterminated {opener} substitution in launch command")]
    UnterminatedSubstitution { opener: &'static str },
}

/// Whether a browser program exposes the Chrome DevTools Protocol on its
/// default profile. Google Chrome and Microsoft Edge refuse
/// `--remote-debugging-port` unless a non-default `--user-data-dir` is given,
/// so they never get a CDP port allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowserKind {
    CdpCapable,
    CdpBlocked,
}

/// Program-name stems for Chromium-family browsers that block CDP on the
/// default profile. A stem matches a program name exactly, or followed by a
/// channel suffix (`google-chrome-stable`) or a version number (`electron33`).
const CDP_BLOCKED_STEMS: &[&str] = &["google-chrome", "chrome", "microsoft-edge", "msedge"];

/// Program-name stems for Chromium-family programs that expose CDP on their
/// default profile.
const CDP_CAPABLE_STEMS: &[&str] = &[
    "chromium",
    "brave",
    "brave-browser",
    "vivaldi",
    "electron",
    "code",
    "codium",
    "vscodium",
];

/// Chromium switches this crate injects, matched against the browser's own
/// arguments so an explicit user value is never duplicated or overridden.
const OZONE_PLATFORM_SWITCH: &str = "--ozone-platform";
const PASSWORD_STORE_SWITCH: &str = "--password-store";

/// A Chromium-family browser found inside a launch command line.
#[derive(Debug, Clone)]
pub(crate) struct BrowserInvocation {
    pub(crate) kind: BrowserKind,
    /// Program name as invoked, with any directory part removed.
    pub(crate) program: String,
    /// Byte offset in the original command just past the browser program word.
    insert_at: usize,
    /// The browser's own argv already sets `--ozone-platform`.
    pub(crate) has_ozone_platform: bool,
    /// The browser's own argv already sets `--password-store`.
    pub(crate) has_password_store: bool,
}

impl BrowserInvocation {
    /// Return `command` with `switches` inserted into the browser's own argv,
    /// immediately after the program word and before its first argument. Every
    /// other byte of `command` is preserved exactly.
    pub(crate) fn with_switches(&self, command: &str, switches: &[String]) -> String {
        if switches.is_empty() {
            return command.to_owned();
        }
        let mut out = String::with_capacity(command.len() + switches.len() * 32);
        out.push_str(&command[..self.insert_at]);
        for switch in switches {
            out.push(' ');
            out.push_str(switch);
        }
        out.push_str(&command[self.insert_at..]);
        out
    }
}

/// Find the first Chromium-family browser command in a shell command line.
///
/// Returns `Ok(None)` when the line launches no such browser, including for
/// commands that merely mention one in a URL, a quoted string, or a filename.
pub(crate) fn find_browser_invocation(
    command: &str,
) -> Result<Option<BrowserInvocation>, CommandParseError> {
    let tokens = lex(command)?;
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index].ends_command() {
            index += 1;
            continue;
        }
        let mut end = index;
        while end < tokens.len() && !tokens[end].ends_command() {
            end += 1;
        }
        if let Some(found) = analyze_command(&tokens[index..end]) {
            return Ok(Some(found));
        }
        index = end;
    }
    Ok(None)
}

/// Inspect one simple command (no control operators) and report the browser it
/// runs, if any.
fn analyze_command(words: &[Token]) -> Option<BrowserInvocation> {
    let mut index = skip_assignments(words, 0);
    if program_name(&words.get(index)?.value) == "env" {
        index = skip_env_options(words, index + 1);
    }
    let program_word = words.get(index)?;
    if program_word.kind != TokenKind::Word {
        return None;
    }
    let program = program_name(&program_word.value);
    let kind = classify_program(&program)?;

    let mut has_ozone_platform = false;
    let mut has_password_store = false;
    let mut redirect_target = false;
    for word in words.iter().skip(index + 1) {
        if word.kind == TokenKind::Operator {
            redirect_target = true;
            continue;
        }
        if redirect_target {
            redirect_target = false;
            continue;
        }
        has_ozone_platform |= sets_switch(&word.value, OZONE_PLATFORM_SWITCH);
        has_password_store |= sets_switch(&word.value, PASSWORD_STORE_SWITCH);
    }

    Some(BrowserInvocation {
        kind,
        program,
        insert_at: program_word.end,
        has_ozone_platform,
        has_password_store,
    })
}

/// Skip leading `NAME=value` environment assignments.
fn skip_assignments(words: &[Token], mut index: usize) -> usize {
    while let Some(word) = words.get(index) {
        if word.kind != TokenKind::Word || !is_assignment(&word.value) {
            break;
        }
        index += 1;
    }
    index
}

/// Skip the options and assignments of an `env` prefix so the real program word
/// is reached. `launch_app` emits such a prefix itself to route the browser at
/// the session service bus.
fn skip_env_options(words: &[Token], mut index: usize) -> usize {
    while let Some(word) = words.get(index) {
        if word.kind != TokenKind::Word {
            break;
        }
        match word.value.as_str() {
            "-i" | "--ignore-environment" | "-" => index += 1,
            "--" => {
                index += 1;
                break;
            }
            "-u" | "--unset" => index += 2,
            value if is_assignment(value) => index += 1,
            _ => break,
        }
    }
    index
}

/// True when `word` is a `NAME=value` shell assignment.
fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Program name with any directory part removed, lowercased for comparison.
fn program_name(word: &str) -> String {
    match word.rsplit_once('/') {
        Some((_, name)) => name.to_lowercase(),
        None => word.to_lowercase(),
    }
}

/// Classify a program name against the Chromium-family stems.
fn classify_program(program: &str) -> Option<BrowserKind> {
    if CDP_BLOCKED_STEMS
        .iter()
        .any(|stem| matches_stem(program, stem))
    {
        return Some(BrowserKind::CdpBlocked);
    }
    if CDP_CAPABLE_STEMS
        .iter()
        .any(|stem| matches_stem(program, stem))
    {
        return Some(BrowserKind::CdpCapable);
    }
    None
}

/// A program name matches a stem exactly, with a `-suffix` channel or variant
/// (`google-chrome-stable`, `chromium-browser`), or with a version number
/// (`electron33`).
fn matches_stem(program: &str, stem: &str) -> bool {
    match program.strip_prefix(stem) {
        Some("") => true,
        Some(rest) => rest.starts_with('-') || rest.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// True when an argument sets `switch`, as `--switch` or `--switch=value`.
fn sets_switch(argument: &str, switch: &str) -> bool {
    match argument.strip_prefix(switch) {
        Some("") => true,
        Some(rest) => rest.starts_with('='),
        None => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    Operator,
}

/// One shell token, with the byte range it occupies in the original command and
/// its quote- and escape-resolved text.
#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    end: usize,
    value: String,
}

impl Token {
    /// True for the control operators that terminate a simple command.
    fn ends_command(&self) -> bool {
        self.kind == TokenKind::Operator
            && matches!(
                self.value.as_str(),
                "&&" | "||" | "|" | "&" | ";" | ";;" | "\n" | "(" | ")"
            )
    }
}

/// Characters that begin a shell operator.
fn is_operator_start(c: char) -> bool {
    matches!(c, '\n' | ';' | '|' | '&' | '<' | '>' | '(' | ')')
}

/// Length in characters of the operator starting at `index`, if any.
fn operator_len(chars: &[(usize, char)], index: usize) -> Option<usize> {
    let current = chars.get(index)?.1;
    if !is_operator_start(current) {
        return None;
    }
    let next = chars.get(index + 1).map(|(_, c)| *c);
    let pair = matches!(
        (current, next),
        ('&', Some('&'))
            | ('|', Some('|'))
            | ('>', Some('>'))
            | ('<', Some('<'))
            | (';', Some(';'))
            | ('&', Some('>'))
            | ('>', Some('|'))
            | ('>', Some('&'))
            | ('<', Some('&'))
            | ('<', Some('>'))
    );
    Some(if pair { 2 } else { 1 })
}

/// Split a command line into words and operators, resolving single quotes,
/// double quotes, and backslash escapes, and keeping `$(…)`, `${…}`, and
/// backtick substitutions intact inside the word that contains them.
fn lex(command: &str) -> Result<Vec<Token>, CommandParseError> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let (offset, c) = chars[index];
        if c.is_whitespace() && c != '\n' {
            index += 1;
            continue;
        }
        if let Some(len) = operator_len(&chars, index) {
            let end = char_end(&chars, command, index + len - 1);
            tokens.push(Token {
                kind: TokenKind::Operator,
                end,
                value: command[offset..end].to_owned(),
            });
            index += len;
            continue;
        }
        let mut value = String::new();
        while index < chars.len() {
            let c = chars[index].1;
            if (c.is_whitespace() && c != '\n') || is_operator_start(c) {
                break;
            }
            index = match c {
                '\'' => read_single_quoted(&chars, index, &mut value)?,
                '"' => read_double_quoted(&chars, index, &mut value)?,
                '\\' => read_escape(&chars, index, &mut value),
                '`' => read_backticks(&chars, index, &mut value)?,
                '$' if chars.get(index + 1).map(|(_, c)| *c) == Some('(') => {
                    read_nested(&chars, index + 1, '(', ')', "$(", &mut value)?
                }
                '$' if chars.get(index + 1).map(|(_, c)| *c) == Some('{') => {
                    read_nested(&chars, index + 1, '{', '}', "${", &mut value)?
                }
                other => {
                    value.push(other);
                    index + 1
                }
            };
        }
        let end = match chars.get(index) {
            Some((next_offset, _)) => *next_offset,
            None => command.len(),
        };
        tokens.push(Token {
            kind: TokenKind::Word,
            end,
            value,
        });
    }
    Ok(tokens)
}

/// Byte offset just past the character at `index`.
fn char_end(chars: &[(usize, char)], command: &str, index: usize) -> usize {
    match chars.get(index) {
        Some((offset, c)) => offset + c.len_utf8(),
        None => command.len(),
    }
}

fn read_single_quoted(
    chars: &[(usize, char)],
    start: usize,
    value: &mut String,
) -> Result<usize, CommandParseError> {
    let mut index = start + 1;
    while let Some((_, c)) = chars.get(index) {
        if *c == '\'' {
            return Ok(index + 1);
        }
        value.push(*c);
        index += 1;
    }
    Err(CommandParseError::UnterminatedQuote { quote: '\'' })
}

fn read_double_quoted(
    chars: &[(usize, char)],
    start: usize,
    value: &mut String,
) -> Result<usize, CommandParseError> {
    let mut index = start + 1;
    while let Some((_, c)) = chars.get(index) {
        match c {
            '"' => return Ok(index + 1),
            '\\' => {
                index = match chars.get(index + 1).map(|(_, c)| *c) {
                    Some(escaped @ ('"' | '\\' | '$' | '`')) => {
                        value.push(escaped);
                        index + 2
                    }
                    Some(other) => {
                        value.push('\\');
                        value.push(other);
                        index + 2
                    }
                    None => index + 1,
                };
            }
            '`' => index = read_backticks(chars, index, value)?,
            '$' if chars.get(index + 1).map(|(_, c)| *c) == Some('(') => {
                index = read_nested(chars, index + 1, '(', ')', "$(", value)?;
            }
            '$' if chars.get(index + 1).map(|(_, c)| *c) == Some('{') => {
                index = read_nested(chars, index + 1, '{', '}', "${", value)?;
            }
            other => {
                value.push(*other);
                index += 1;
            }
        }
    }
    Err(CommandParseError::UnterminatedQuote { quote: '"' })
}

fn read_escape(chars: &[(usize, char)], start: usize, value: &mut String) -> usize {
    match chars.get(start + 1) {
        Some((_, escaped)) => {
            value.push(*escaped);
            start + 2
        }
        None => start + 1,
    }
}

fn read_backticks(
    chars: &[(usize, char)],
    start: usize,
    value: &mut String,
) -> Result<usize, CommandParseError> {
    value.push('`');
    let mut index = start + 1;
    while let Some((_, c)) = chars.get(index) {
        value.push(*c);
        if *c == '`' {
            return Ok(index + 1);
        }
        index += 1;
    }
    Err(CommandParseError::UnterminatedSubstitution { opener: "`" })
}

/// Copy a balanced `open`/`close` substitution verbatim into the current word.
/// `start` is the index of the opening bracket and `opener` is the introducer
/// text (`$(`, `${`) that precedes it.
fn read_nested(
    chars: &[(usize, char)],
    start: usize,
    open: char,
    close: char,
    opener: &'static str,
    value: &mut String,
) -> Result<usize, CommandParseError> {
    value.push_str(opener);
    let mut depth = 1_usize;
    let mut index = start + 1;
    while let Some((_, c)) = chars.get(index) {
        value.push(*c);
        if *c == open {
            depth += 1;
        } else if *c == close {
            depth -= 1;
            if depth == 0 {
                return Ok(index + 1);
            }
        }
        index += 1;
    }
    Err(CommandParseError::UnterminatedSubstitution { opener })
}

#[cfg(test)]
mod tests {
    use super::{BrowserKind, CommandParseError, find_browser_invocation};

    fn browser(command: &str) -> super::BrowserInvocation {
        match find_browser_invocation(command) {
            Ok(Some(found)) => found,
            Ok(None) => panic!("no browser found in {command:?}"),
            Err(error) => panic!("parse error for {command:?}: {error}"),
        }
    }

    fn no_browser(command: &str) {
        match find_browser_invocation(command) {
            Ok(None) => (),
            Ok(Some(found)) => panic!("unexpected browser {} in {command:?}", found.program),
            Err(error) => panic!("parse error for {command:?}: {error}"),
        }
    }

    fn rewritten(command: &str, switches: &[&str]) -> String {
        let switches: Vec<String> = switches.iter().map(|s| (*s).to_owned()).collect();
        browser(command).with_switches(command, &switches)
    }

    #[test]
    fn switches_land_on_chrome_not_on_a_later_command() {
        assert_eq!(
            rewritten(
                "google-chrome-stable https://example.com && echo launched",
                &["--ozone-platform=wayland", "--password-store=kwallet6"],
            ),
            "google-chrome-stable --ozone-platform=wayland --password-store=kwallet6 \
             https://example.com && echo launched"
        );
    }

    #[test]
    fn browser_is_found_after_an_unrelated_command() {
        let found = browser("echo starting; chromium https://example.com");
        assert_eq!(found.program, "chromium");
        assert_eq!(found.kind, BrowserKind::CdpCapable);
        assert_eq!(
            rewritten(
                "echo starting; chromium https://example.com",
                &["--password-store=kwallet6"]
            ),
            "echo starting; chromium --password-store=kwallet6 https://example.com"
        );
    }

    #[test]
    fn a_url_mentioning_the_switch_does_not_suppress_injection() {
        let found = browser("google-chrome-stable 'https://example.com/#--password-store=basic'");
        assert!(!found.has_password_store);
        assert!(!found.has_ozone_platform);
    }

    #[test]
    fn a_later_command_mentioning_the_switch_does_not_suppress_injection() {
        let found =
            browser("google-chrome-stable https://example.com && echo --password-store=basic");
        assert!(!found.has_password_store);
    }

    #[test]
    fn an_explicit_switch_on_the_browser_is_respected() {
        let found =
            browser("chromium --password-store=basic --ozone-platform=x11 https://example.com");
        assert!(found.has_password_store);
        assert!(found.has_ozone_platform);
    }

    #[test]
    fn chrome_and_edge_are_reported_as_cdp_blocked() {
        assert_eq!(
            browser("google-chrome-stable").kind,
            BrowserKind::CdpBlocked
        );
        assert_eq!(
            browser("microsoft-edge-stable").kind,
            BrowserKind::CdpBlocked
        );
        assert_eq!(
            browser("/opt/google/chrome/chrome").kind,
            BrowserKind::CdpBlocked
        );
    }

    #[test]
    fn non_browser_commands_are_not_treated_as_chromium() {
        no_browser("kate /tmp/code.txt");
        no_browser("konsole");
        no_browser("firefox https://example.com");
        no_browser("echo 'chromium https://example.com'");
        no_browser("echo \"launch chromium later\"");
    }

    #[test]
    fn assignments_and_env_prefixes_are_skipped() {
        assert_eq!(
            browser("LANG=C chromium https://example.com").program,
            "chromium"
        );
        assert_eq!(
            browser("env DBUS_SESSION_BUS_ADDRESS='unix:path=/tmp/bus' google-chrome-stable")
                .program,
            "google-chrome-stable"
        );
        assert_eq!(
            rewritten(
                "env FOO=1 chromium https://example.com",
                &["--ozone-platform=wayland"]
            ),
            "env FOO=1 chromium --ozone-platform=wayland https://example.com"
        );
    }

    #[test]
    fn redirections_stay_attached_to_the_browser_command() {
        assert_eq!(
            rewritten(
                "chromium https://example.com > /tmp/out.log 2>&1",
                &["--password-store=kwallet6"]
            ),
            "chromium --password-store=kwallet6 https://example.com > /tmp/out.log 2>&1"
        );
    }

    #[test]
    fn quoted_program_words_keep_their_quotes() {
        assert_eq!(
            rewritten(
                "\"google-chrome-stable\" https://example.com",
                &["--ozone-platform=wayland"]
            ),
            "\"google-chrome-stable\" --ozone-platform=wayland https://example.com"
        );
    }

    #[test]
    fn unterminated_quotes_are_reported() {
        match find_browser_invocation("google-chrome-stable 'https://example.com") {
            Err(CommandParseError::UnterminatedQuote { quote }) => assert_eq!(quote, '\''),
            Err(error) => panic!("unexpected error: {error}"),
            Ok(_) => panic!("expected an unterminated quote error"),
        }
    }
}
