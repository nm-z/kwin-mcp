//! How accessibility_tree and the element search name what they report.
//!
//! Both tools read the same objects, but a caller copies a name out of one and
//! types it into the other, and three things made that fail (#891):
//!
//! - the text is not typed back the way it was printed: the tree prints a line
//!   break as `\n`, a page writes `…`, `’` or a no-break space where a caller
//!   types `...`, `'` or a space, and rich-text fields carry invisible
//!   characters. `text_key` folds both sides to one spelling;
//! - the two sources name roles differently: AT-SPI says `entry`, `push button`
//!   and `check box` where the page's own tree says `textbox`, `button` and
//!   `checkbox`, and the tree prints the role before the name. `role_matches`
//!   and `query_matches` accept either vocabulary, and a query that starts with
//!   the role;
//! - a page renames the element between the two calls. Perplexity's composer
//!   sets `aria-label` and `aria-placeholder` to a new hint every few seconds
//!   ("Ask anything…", "Type / for search modes", "Type @ for connectors"), so
//!   the name the tree printed was already gone when it was searched for.
//!   `SeenNames` remembers the names each object was reported under, and a
//!   search by one of them still reaches the object, which is then returned
//!   with the name it has now.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a name an object was reported under still leads to it.
const NAME_MEMORY: Duration = Duration::from_secs(300);
/// Objects remembered at once; a page of a few thousand nodes fits.
const MAX_OBJECTS: usize = 8192;
/// Earlier names kept per object. A rotating hint has a handful.
const MAX_NAMES: usize = 8;

/// `text` as both tools compare it: lower case, typographic punctuation in its
/// ASCII spelling, invisible characters removed, and every run of white space
/// (also the `\n`, `\r` and `\t` the tree prints for it) as one space.
pub fn text_key(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' if matches!(characters.peek(), Some('n' | 'r' | 't')) => {
                characters.next();
                folded.push(' ');
            }
            '…' => folded.push_str("..."),
            '‘' | '’' | '‚' | '′' => folded.push('\''),
            '“' | '”' | '„' | '″' => folded.push('"'),
            '‐' | '‑' | '‒' | '–' | '—' | '−' => folded.push('-'),
            // Zero-width spaces and joiners, the word joiner, the byte order mark,
            // soft hyphens, and the object replacement character a rich-text
            // field reports for an embedded element.
            '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}' | '\u{ad}' | '\u{fffc}' => {}
            other => folded.push(other),
        }
    }
    folded
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Names the other source uses for the same role. AT-SPI role names are on the
/// left of each pair, the page's (ARIA) names on the right.
const ROLE_PAIRS: [(&str, &str); 8] = [
    ("entry", "textbox"),
    ("password text", "textbox"),
    ("push button", "button"),
    ("toggle button", "button"),
    ("check box", "checkbox"),
    ("radio button", "radio"),
    ("combo box", "combobox"),
    ("menu item", "menuitem"),
];

/// `role` (already a `text_key`) and what the other source calls it.
fn role_names(role: &str) -> impl Iterator<Item = &str> {
    std::iter::once(role).chain(ROLE_PAIRS.iter().filter_map(move |(native, page)| {
        if *native == role {
            Some(*page)
        } else if *page == role {
            Some(*native)
        } else {
            None
        }
    }))
}

/// Roles of words shown on the screen. A caller asks for them as `text` (#882);
/// AT-SPI's own `text` role is an editable area.
const SHOWN_TEXT_ROLES: [&str; 5] = ["static", "label", "paragraph", "heading", "caption"];

/// Whether an element of role `actual` passes a `role` filter of `wanted`, in
/// either source's vocabulary. A filter of `text` also passes shown words.
pub fn role_matches(wanted: Option<&str>, actual: &str) -> bool {
    let Some(wanted) = wanted else {
        return true;
    };
    let wanted = text_key(wanted);
    let actual = text_key(actual);
    role_names(&actual).any(|name| name == wanted)
        || wanted == "text" && SHOWN_TEXT_ROLES.contains(&actual.as_str())
}

/// Whether an element is what a query asks for. `matches` compares one
/// `text_key` with the query in the caller's mode (contains, whole word,
/// exact). The query may be the name, the role in either vocabulary, or both
/// as the tree prints them, role first.
pub fn query_matches(matches: &dyn Fn(&str) -> bool, name: &str, role: &str) -> bool {
    let name = text_key(name);
    let role = text_key(role);
    matches(&name)
        || role_names(&role)
            .any(|role| matches(role) || (!name.is_empty() && matches(&format!("{role} {name}"))))
}

/// The names each accessibility object was reported under, newest last.
#[derive(Default)]
pub struct SeenNames {
    objects: HashMap<String, Vec<(String, Instant)>>,
}

impl SeenNames {
    /// Note that `object` (its bus name and path) was reported as `name`.
    pub fn record(&mut self, object: &str, name: &str, now: Instant) {
        if name.is_empty() {
            return;
        }
        if self.objects.len() >= MAX_OBJECTS && !self.objects.contains_key(object) {
            self.objects.retain(|_, names| {
                names
                    .last()
                    .is_some_and(|(_, seen)| now.duration_since(*seen) < NAME_MEMORY)
            });
            if self.objects.len() >= MAX_OBJECTS {
                self.objects.clear();
            }
        }
        let names = self.objects.entry(object.to_owned()).or_default();
        names.retain(|(known, _)| known != name);
        names.push((name.to_owned(), now));
        if names.len() > MAX_NAMES {
            names.remove(0);
        }
    }

    /// The most recent name `object` was reported under that it no longer has
    /// and that satisfies `wanted`.
    pub fn earlier_name(
        &self,
        object: &str,
        current: &str,
        now: Instant,
        wanted: &dyn Fn(&str) -> bool,
    ) -> Option<&str> {
        self.objects
            .get(object)?
            .iter()
            .rev()
            .filter(|(name, seen)| {
                name != current && now.duration_since(*seen) < NAME_MEMORY && wanted(name)
            })
            .map(|(name, _)| name.as_str())
            .next()
    }
}

#[cfg(test)]
mod tests {
    use super::{NAME_MEMORY, SeenNames, query_matches, role_matches, text_key};
    use std::time::{Duration, Instant};

    #[test]
    fn a_name_typed_from_the_tree_has_the_key_of_the_name_on_the_page() {
        // The composer of #891, as the tree printed it and as a caller types it.
        assert_eq!(text_key("Ask anything…"), text_key("ask anything..."));
        assert_eq!(text_key("Don’t\u{a0}show “tips”"), "don't show \"tips\"");
        // The tree prints a line break as backslash-n.
        assert_eq!(
            text_key("First line\nSecond\tline"),
            text_key("First line\\nSecond\\tline")
        );
        // A rich-text field wraps its text in invisible characters.
        assert_eq!(
            text_key("\u{feff}Message\u{200b} body\u{fffc}"),
            "message body"
        );
        assert_eq!(text_key("2021–2026 — totals"), "2021-2026 - totals");
    }

    #[test]
    fn roles_match_in_either_vocabulary() {
        assert!(role_matches(None, "entry"));
        assert!(role_matches(Some("textbox"), "entry"));
        assert!(role_matches(Some("entry"), "textbox"));
        assert!(role_matches(Some("Button"), "push button"));
        assert!(role_matches(Some("button"), "toggle button"));
        assert!(role_matches(Some("push button"), "button"));
        assert!(role_matches(Some("checkbox"), "check box"));
        assert!(!role_matches(Some("push button"), "toggle button"));
        assert!(!role_matches(Some("link"), "entry"));
    }

    #[test]
    fn a_query_may_be_the_name_the_role_or_the_tree_line() {
        let contains = |query: &'static str| move |text: &str| text.contains(query);
        assert!(query_matches(
            &contains("ask anything..."),
            "Ask anything…",
            "entry"
        ));
        assert!(query_matches(
            &contains("textbox"),
            "Ask anything…",
            "entry"
        ));
        assert!(query_matches(
            &contains("entry ask anything..."),
            "Ask anything…",
            "entry"
        ));
        assert!(query_matches(
            &contains("textbox ask anything..."),
            "Ask anything…",
            "entry"
        ));
        assert!(!query_matches(
            &contains("link ask"),
            "Ask anything…",
            "entry"
        ));
        let exact = |query: &'static str| move |text: &str| text == query;
        assert!(query_matches(&exact("button"), "Send", "push button"));
        assert!(query_matches(
            &exact("push button send"),
            "Send",
            "push button"
        ));
        assert!(!query_matches(&exact("sen"), "Send", "push button"));
    }

    #[test]
    fn an_object_is_still_reached_by_a_name_it_was_reported_under() {
        let start = Instant::now();
        let composer = ":1.42/org/a11y/atspi/accessible/77";
        let mut seen = SeenNames::default();
        seen.record(composer, "Ask anything…", start);
        let asked = |name: &str| text_key(name).contains("ask anything");
        // Seconds later the page shows another hint in the same field.
        let later = start + Duration::from_secs(4);
        assert_eq!(
            seen.earlier_name(composer, "Type / for search modes", later, &asked),
            Some("Ask anything…")
        );
        // The name it still has is an ordinary match, not an earlier one.
        assert_eq!(
            seen.earlier_name(composer, "Ask anything…", later, &asked),
            None
        );
        assert_eq!(
            seen.earlier_name(":1.42/other", "Type", later, &asked),
            None
        );
        seen.record(composer, "Type / for search modes", later);
        seen.record(
            composer,
            "Type @ for connectors",
            later + Duration::from_secs(4),
        );
        assert_eq!(
            seen.earlier_name(
                composer,
                "Type @ for connectors",
                later + Duration::from_secs(5),
                &asked
            ),
            Some("Ask anything…")
        );
        // A name is forgotten once it is old: the path may be another element's by then.
        assert_eq!(
            seen.earlier_name(
                composer,
                "Type @ for connectors",
                start + NAME_MEMORY,
                &asked
            ),
            None
        );
    }
}
