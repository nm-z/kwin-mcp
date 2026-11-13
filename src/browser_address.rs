//! What `browser_navigate` relies on instead of the window title: the address a
//! caller asked for as Chrome opens it, and, through AT-SPI, a Chromium window's
//! address bar, its selected tab and the address of the page it shows.

use atspi::proxy::accessible::ObjectRefExt;
use atspi::proxy::proxy_ext::ProxyExt;
use std::time::Duration;

/// Chrome does not navigate to a longer URL (`url::kMaxURLChars`). Chrome 156
/// opens 2097152 characters through CDP and aborts 2097153.
pub const URL_MAX_CHARS: usize = 2 * 1024 * 1024;
/// The address bar keeps this much of a paste: Chrome 156 kept 512000 of
/// 2097152 pasted characters and opened the shortened address.
pub const ADDRESS_BAR_MAX_CHARS: usize = 512_000;
/// A longer address is pasted into the address bar instead of typed key by key:
/// paced typing takes about 110 ms per character.
pub const TYPED_MAX_CHARS: usize = 128;
/// A longer address is shortened in a result or an error. Up to here it is whole,
/// so a caller can open it again.
const SHOWN_URL_CHARS: usize = 2000;

/// Chromium's Views class of the address bar; its accessible name is translated.
pub const ADDRESS_BAR_CLASS: &str = "OmniboxViewViews";
/// The address of the page Chrome shows when an address did not load.
const ERROR_PAGE_SCHEME: &str = "chrome-error:";
const UI_SURVEY_TIMEOUT: Duration = Duration::from_secs(3);
const UI_READ_TIMEOUT: Duration = Duration::from_secs(1);
const UI_SURVEY_NODES: usize = 4000;
/// Controls of the browser's own interface that hold no address bar, tab strip
/// or page below them.
const UI_LEAF_ROLES: [atspi::Role; 13] = [
    atspi::Role::Button,
    atspi::Role::ToggleButton,
    atspi::Role::Static,
    atspi::Role::Label,
    atspi::Role::Notification,
    atspi::Role::Separator,
    atspi::Role::PageTab,
    atspi::Role::Image,
    atspi::Role::Link,
    atspi::Role::Menu,
    atspi::Role::MenuBar,
    atspi::Role::MenuItem,
    atspi::Role::Text,
];

/// The reason `url` cannot be opened, if any.
pub fn url_problem(url: &str) -> Option<String> {
    let characters = url.chars().count();
    if url.is_empty() {
        Some("the url is empty".to_owned())
    } else if url.chars().any(|ch| !ch.is_ascii_graphic()) {
        Some(
            "the url must be printable ASCII without spaces (percent-encode anything else)"
                .to_owned(),
        )
    } else if characters > URL_MAX_CHARS {
        Some(format!(
            "the url has {characters} characters and Chrome opens at most {URL_MAX_CHARS}"
        ))
    } else {
        None
    }
}

/// The length of `text` up to the first of `stops`.
fn length_before(text: &str, stops: &[char]) -> usize {
    text.split(stops).next().map_or(text.len(), str::len)
}

/// The scheme `url` starts with. `localhost:8080/x` names a host and a port,
/// as it does in the address bar.
fn scheme(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once(':')?;
    let mut characters = scheme.chars();
    let named = characters.next().is_some_and(|ch| ch.is_ascii_alphabetic())
        && characters.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-'));
    let port = &rest[..length_before(rest, &['/', '?', '#'])];
    let is_port =
        !port.is_empty() && port.len() <= 5 && port.bytes().all(|byte| byte.is_ascii_digit());
    (named && !is_port).then_some(scheme)
}

/// `url` with the scheme the address is opened with. Without one the address bar
/// decides between an address and a search by the text's shape, so a bare host
/// gets `http://` (Chrome upgrades it to https where the site answers there) and
/// a leading `/` is a file path.
pub fn absolute_url(url: &str) -> String {
    if scheme(url).is_some() {
        url.to_owned()
    } else if url.starts_with("//") {
        format!("http:{url}")
    } else if url.starts_with('/') {
        format!("file://{url}")
    } else {
        format!("http://{url}")
    }
}

/// Whether the address bar opens `url` as an address whatever follows the scheme.
/// With any other scheme it may run a search for the text instead.
fn always_navigated(url: &str) -> bool {
    scheme(url).is_some_and(|scheme| {
        [
            "http",
            "https",
            "file",
            "about",
            "data",
            "blob",
            "view-source",
            "chrome",
            "chrome-extension",
            "devtools",
            "edge",
            "brave",
            "vivaldi",
        ]
        .iter()
        .any(|known| scheme.eq_ignore_ascii_case(known))
    })
}

/// Whether the page at `landed` is the one that was asked for: same host, path,
/// query and fragment, ignoring the scheme, a leading `www.` and a trailing path
/// slash. Paths, queries and fragments stay case-sensitive.
pub fn same_destination(requested: &str, landed: &str) -> bool {
    let normalize = |url: &str| {
        let url = url.split_once("://").map_or(url, |(_, rest)| rest);
        let (host, rest) = url.split_at(length_before(url, &['/', '?', '#']));
        let (path, suffix) = rest.split_at(length_before(rest, &['?', '#']));
        format!(
            "{}{}{}",
            host.to_ascii_lowercase().trim_start_matches("www."),
            path.trim_end_matches('/'),
            suffix
        )
    };
    normalize(requested) == normalize(landed)
}

/// Whether `landed` is a search for the text `entered` in the address bar: the
/// text is not an address the bar always opens, and the query of `landed`
/// carries it.
pub fn searched_for(entered: &str, landed: &str) -> bool {
    if always_navigated(entered) {
        return false;
    }
    let Some((_, query)) = landed.split_once('?') else {
        return false;
    };
    let mut decoded = Vec::with_capacity(query.len());
    let bytes = query.as_bytes();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let escaped = (byte == b'%')
            .then(|| query.get(index + 1..index + 3))
            .flatten()
            .and_then(|digits| u8::from_str_radix(digits, 16).ok());
        match escaped {
            Some(value) => {
                decoded.push(value);
                index += 3;
            }
            None => {
                decoded.push(if byte == b'+' { b' ' } else { byte });
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).contains(entered)
}

/// Whether `uri` is Chrome's page for an address that did not load.
pub fn is_error_page(uri: &str) -> bool {
    uri.starts_with(ERROR_PAGE_SCHEME)
}

/// `url` for a result: whole when short, else its start and its length.
pub fn shown_url(url: &str) -> String {
    let characters = url.chars().count();
    if characters <= SHOWN_URL_CHARS {
        return url.to_owned();
    }
    let head: String = url.chars().take(SHOWN_URL_CHARS).collect();
    format!("{head}…[{characters} characters]")
}

/// Whether a KWin caption is that of a window whose active tab has `title`:
/// the title alone, or followed by the browser's own segments.
pub fn caption_names_tab(caption: &str, title: &str) -> bool {
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let (caption, title) = (collapse(caption), collapse(title));
    !title.is_empty()
        && caption
            .strip_prefix(&title)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(" - "))
}

/// The page a browser window shows.
#[derive(Clone, Debug)]
pub struct ShownPage {
    /// The address the document was loaded from. Empty for the blank page and
    /// for a document whose content has not arrived; a navigation inside the
    /// document (another fragment) does not change it.
    pub uri: String,
    pub loading: bool,
    /// The accessibility object of the document. A navigation replaces it, also
    /// when the address stays the same: a reload, or one error page after another.
    document: String,
}

impl ShownPage {
    /// Whether this is another document or another address than `earlier`.
    pub fn replaced(&self, earlier: &Self) -> bool {
        self.uri != earlier.uri || self.document != earlier.document
    }

    /// This page under the address `uri`, where the document does not report it.
    pub fn at(self, uri: &str) -> Self {
        Self {
            uri: uri.to_owned(),
            ..self
        }
    }
}

/// Whether `requested` is an address inside the document loaded from `shown`:
/// the same destination once both fragments are set aside.
pub fn same_document(requested: &str, shown: &str) -> bool {
    let without_fragment = |url: &str| url[..length_before(url, &['#'])].to_owned();
    !shown.is_empty() && same_destination(&without_fragment(requested), &without_fragment(shown))
}

#[derive(Debug)]
pub enum UiError {
    Bus(String),
    NoWindow,
    NoAddressBar,
    Ambiguous(usize),
    TimedOut,
}

impl std::fmt::Display for UiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bus(error) => write!(formatter, "the accessibility bus failed: {error}"),
            Self::NoWindow => write!(
                formatter,
                "accessibility lists no browser window with the active window's title"
            ),
            Self::NoAddressBar => write!(
                formatter,
                "the browser window exposes no {ADDRESS_BAR_CLASS} address bar through accessibility"
            ),
            Self::Ambiguous(count) => write!(
                formatter,
                "{count} browser windows of this process share the active window's title and none is marked active"
            ),
            Self::TimedOut => write!(
                formatter,
                "the browser did not answer accessibility queries within {} s",
                UI_SURVEY_TIMEOUT.as_secs()
            ),
        }
    }
}

#[derive(Default)]
struct Survey {
    address_bar: Option<atspi::ObjectRefOwned>,
    tabs: Option<atspi::ObjectRefOwned>,
    page: Option<(ShownPage, Option<atspi::ObjectRefOwned>)>,
}

/// One browser window's address bar, tab strip and page, bound to accessibility
/// objects so that a later title change or another window's activation cannot
/// redirect the reads.
pub struct BrowserUi {
    bus: zbus::Connection,
    frame: atspi::ObjectRefOwned,
    address_bar: atspi::ObjectRefOwned,
    tabs: Option<atspi::ObjectRefOwned>,
    /// The parent of the page seen last, which a navigation keeps.
    page_parent: std::sync::Mutex<Option<atspi::ObjectRefOwned>>,
}

async fn shown_page(
    node: &atspi::proxy::accessible::AccessibleProxy<'_>,
    states: atspi::StateSet,
) -> Option<ShownPage> {
    if !states.contains(atspi::State::Showing) {
        return None;
    }
    let uri = node
        .proxies()
        .await
        .ok()?
        .document()
        .await
        .ok()?
        .get_attribute_value("URI")
        .await
        .ok()?;
    Some(ShownPage {
        uri,
        loading: states.contains(atspi::State::Busy),
        document: node.inner().path().to_string(),
    })
}

/// Walk the showing part of `frame` for its address bar, tab strip and page.
/// Controls and the page's own content are not entered.
async fn survey(bus: &zbus::Connection, frame: &atspi::ObjectRefOwned, page_only: bool) -> Survey {
    let mut seen = Survey::default();
    let mut stack: Vec<(atspi::ObjectRefOwned, Option<atspi::ObjectRefOwned>)> =
        vec![(frame.clone(), None)];
    let mut visited = 0usize;
    while let Some((object, parent)) = stack.pop() {
        visited += 1;
        if visited > UI_SURVEY_NODES {
            break;
        }
        let Ok(node) = object.as_accessible_proxy(bus).await else {
            continue;
        };
        let Ok(role) = node.get_role().await else {
            continue;
        };
        let states = node.get_state().await.unwrap_or_default();
        if role == atspi::Role::DocumentWeb {
            if seen.page.is_none()
                && let Some(page) = shown_page(&node, states).await
            {
                seen.page = Some((page, parent));
                if page_only {
                    break;
                }
            }
        } else if role == atspi::Role::Entry {
            if seen.address_bar.is_none()
                && node.get_attributes().await.is_ok_and(|attributes| {
                    attributes.get("class").map(String::as_str) == Some(ADDRESS_BAR_CLASS)
                })
            {
                seen.address_bar = Some(object);
            }
        } else if role == atspi::Role::PageTabList {
            if seen.tabs.is_none() {
                seen.tabs = Some(object);
            }
        } else if !UI_LEAF_ROLES.contains(&role) {
            // The frame itself is entered whatever it reports; below it a part
            // that is not showing holds nothing this window displays.
            if parent.is_some() && !states.contains(atspi::State::Showing) {
                continue;
            }
            for child in node
                .get_children()
                .await
                .unwrap_or_default()
                .into_iter()
                .rev()
            {
                stack.push((child, Some(object.clone())));
            }
        }
    }
    seen
}

impl BrowserUi {
    /// Bind to the window of process `pid` whose caption is `caption`. `pid` is the
    /// window's client as KWin reports it; the accessibility bus sees the same
    /// process namespace.
    pub async fn locate(address: &str, caption: &str, pid: i32) -> Result<Self, UiError> {
        tokio::time::timeout(
            UI_SURVEY_TIMEOUT,
            Self::locate_unbounded(address, caption, pid),
        )
        .await
        .map_err(|_| UiError::TimedOut)?
    }

    async fn locate_unbounded(address: &str, caption: &str, pid: i32) -> Result<Self, UiError> {
        let bus_error = |error: zbus::Error| UiError::Bus(error.to_string());
        let bus = zbus::connection::Builder::address(address)
            .map_err(bus_error)?
            .auth_mechanism(zbus::AuthMechanism::Anonymous)
            .build()
            .await
            .map_err(bus_error)?;
        let root = atspi::proxy::accessible::AccessibleProxy::builder(&bus)
            .destination("org.a11y.atspi.Registry")
            .map_err(bus_error)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(bus_error)?;
        let daemon = zbus::fdo::DBusProxy::new(&bus).await.map_err(bus_error)?;
        let mut frames = Vec::new();
        for app in root.get_children().await.map_err(bus_error)? {
            let Some(owner) = app.name().cloned() else {
                continue;
            };
            let owner_pid = daemon
                .get_connection_unix_process_id(zbus::names::BusName::Unique(owner))
                .await
                .ok();
            if owner_pid.is_some_and(|owner_pid| i64::from(owner_pid) != i64::from(pid)) {
                continue;
            }
            let Ok(app) = app.as_accessible_proxy(&bus).await else {
                continue;
            };
            for frame in app.get_children().await.unwrap_or_default() {
                let Ok(node) = frame.as_accessible_proxy(&bus).await else {
                    continue;
                };
                if crate::input_bridge::caption_starts_with(
                    &node.name().await.unwrap_or_default(),
                    caption,
                ) {
                    let active = node
                        .get_state()
                        .await
                        .is_ok_and(|states| states.contains(atspi::State::Active));
                    frames.push((frame, active));
                }
            }
        }
        if frames.len() > 1 && frames.iter().filter(|(_, active)| *active).count() == 1 {
            frames.retain(|(_, active)| *active);
        }
        let frame = match frames.len() {
            0 => return Err(UiError::NoWindow),
            1 => frames.swap_remove(0).0,
            count => return Err(UiError::Ambiguous(count)),
        };
        let seen = survey(&bus, &frame, false).await;
        let address_bar = seen.address_bar.ok_or(UiError::NoAddressBar)?;
        Ok(Self {
            bus,
            frame,
            address_bar,
            tabs: seen.tabs,
            page_parent: std::sync::Mutex::new(seen.page.and_then(|(_, parent)| parent)),
        })
    }

    /// Whether the address bar holds the window's keyboard focus.
    pub async fn address_focused(&self) -> bool {
        let read = async {
            let node = self.address_bar.as_accessible_proxy(&self.bus).await.ok()?;
            node.get_state().await.ok()
        };
        tokio::time::timeout(UI_READ_TIMEOUT, read)
            .await
            .ok()
            .flatten()
            .is_some_and(|states| states.contains(atspi::State::Focused))
    }

    /// Ask the browser to focus its address bar. Unlike ctrl+l this does not
    /// pass through the page, which can delay or consume the shortcut.
    pub async fn focus_address(&self) -> bool {
        let focus = async {
            let node = self.address_bar.as_accessible_proxy(&self.bus).await.ok()?;
            node.proxies()
                .await
                .ok()?
                .component()
                .await
                .ok()?
                .grab_focus()
                .await
                .ok()
        };
        tokio::time::timeout(UI_READ_TIMEOUT, focus)
            .await
            .ok()
            .flatten()
            == Some(true)
    }

    /// The whole text of the address bar. `wait` bounds the read: a pasted
    /// address of hundreds of thousands of characters takes longer than a typed one.
    pub async fn address_text(&self, wait: Duration) -> Option<String> {
        let read = async {
            let node = self.address_bar.as_accessible_proxy(&self.bus).await.ok()?;
            let text = node.proxies().await.ok()?.text().await.ok()?;
            let characters = text.character_count().await.ok()?;
            text.get_text(0, characters).await.ok()
        };
        tokio::time::timeout(wait, read).await.ok().flatten()
    }

    /// The accessibility object of the selected tab, which a tab keeps while it
    /// navigates.
    pub async fn selected_tab(&self) -> Option<String> {
        let tabs = self.tabs.as_ref()?;
        let read = async {
            let list = tabs.as_accessible_proxy(&self.bus).await.ok()?;
            for tab in list.get_children().await.ok()? {
                let Ok(node) = tab.as_accessible_proxy(&self.bus).await else {
                    continue;
                };
                if node
                    .get_state()
                    .await
                    .is_ok_and(|states| states.contains(atspi::State::Selected))
                {
                    return Some(tab.path_as_str().to_owned());
                }
            }
            None
        };
        tokio::time::timeout(UI_READ_TIMEOUT, read)
            .await
            .ok()
            .flatten()
    }

    /// The page the window shows now.
    pub async fn page(&self) -> Option<ShownPage> {
        let read = async {
            let parent = self.page_parent.lock().ok()?.clone();
            if let Some(parent) = parent
                && let Ok(node) = parent.as_accessible_proxy(&self.bus).await
            {
                for child in node.get_children().await.unwrap_or_default() {
                    let Ok(child) = child.as_accessible_proxy(&self.bus).await else {
                        continue;
                    };
                    if child.get_role().await.ok() == Some(atspi::Role::DocumentWeb)
                        && let Some(page) =
                            shown_page(&child, child.get_state().await.unwrap_or_default()).await
                    {
                        return Some(page);
                    }
                }
            }
            let (page, parent) = survey(&self.bus, &self.frame, true).await.page?;
            if let Ok(mut known) = self.page_parent.lock() {
                *known = parent;
            }
            Some(page)
        };
        tokio::time::timeout(UI_READ_TIMEOUT, read)
            .await
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ADDRESS_BAR_MAX_CHARS, URL_MAX_CHARS, absolute_url, caption_names_tab, is_error_page,
        same_destination, same_document, searched_for, shown_url, url_problem,
    };

    #[test]
    fn urls_are_checked_for_what_chrome_can_open() {
        assert!(url_problem("learn.zybooks.com/zybook/X/chapter/3/section/14").is_none());
        assert!(url_problem("https://example.test/a?b=c#d").is_none());
        assert!(url_problem("").is_some());
        assert!(url_problem("two words").is_some());
        assert!(url_problem("caf\u{e9}.test").is_some());
        // #873: 2000 characters was this tool's limit, not Chrome's.
        assert!(url_problem(&format!("data:text/html,{}", "a".repeat(2001))).is_none());
        assert!(url_problem(&"a".repeat(URL_MAX_CHARS)).is_none());
        assert!(url_problem(&"a".repeat(URL_MAX_CHARS + 1)).is_some());
        assert!(ADDRESS_BAR_MAX_CHARS < URL_MAX_CHARS);
    }

    #[test]
    fn an_address_without_a_scheme_is_never_left_to_the_search_heuristic() {
        // #865: a router address must not reach the address bar in a form it may search for.
        assert_eq!(absolute_url("192.168.1.1/path"), "http://192.168.1.1/path");
        assert_eq!(absolute_url("router.local"), "http://router.local");
        assert_eq!(absolute_url("router"), "http://router");
        assert_eq!(
            absolute_url("localhost:8090/v1/blocks"),
            "http://localhost:8090/v1/blocks"
        );
        assert_eq!(absolute_url("192.168.1.211:8090"), "http://192.168.1.211:8090");
        assert_eq!(absolute_url("//cdn.test/x"), "http://cdn.test/x");
        assert_eq!(absolute_url("/tmp/page.html"), "file:///tmp/page.html");
        for kept in [
            "http://192.168.1.211:8090/v1/blocks",
            "HTTPS://Example.test",
            "file:///tmp/a.html",
            "about:blank",
            "chrome://extensions",
            "data:text/html,<p>x",
            "view-source:http://example.test/",
            "mailto:someone@example.test",
        ] {
            assert_eq!(absolute_url(kept), kept);
        }
    }

    #[test]
    fn a_landing_is_the_destination_only_with_its_path_query_and_fragment() {
        assert!(same_destination(
            "http://learn.zybooks.com/zybook/X/chapter/3/section/14",
            "https://learn.zybooks.com/zybook/X/chapter/3/section/14"
        ));
        assert!(!same_destination(
            "http://learn.zybooks.com/zybook/X/chapter/3/section/1",
            "https://learn.zybooks.com/zybook/X/chapter/3/section/14"
        ));
        assert!(same_destination(
            "http://example.test/a?x=1#top",
            "https://www.example.test/a/?x=1#top"
        ));
        assert!(!same_destination(
            "http://example.test/a?x=New",
            "https://example.test/a?x=old"
        ));
        assert!(!same_destination(
            "http://example.test/Part",
            "https://example.test/part"
        ));
        assert!(!same_destination(
            "http://example.test/a#new",
            "https://example.test/a#old"
        ));
        assert!(same_destination("HTTPS://Example.test", "https://example.test/"));
        assert!(!same_destination(
            "https://www.digikey.com/en/products/result?keywords=TwinMax",
            "https://www.digikey.com/en/products/filter/coaxial-cables-rf/475?s=N4Ig"
        ));
        // Another fragment is the same document, which keeps reporting the
        // address it was loaded from; another query or path is not.
        assert!(same_document("http://example.test/a#two", "http://example.test/a#one"));
        assert!(same_document("http://example.test/a#two", "https://example.test/a"));
        assert!(!same_document("http://example.test/a?x=2#two", "http://example.test/a?x=1#two"));
        assert!(!same_document("http://example.test/b#one", "http://example.test/a#one"));
        assert!(!same_document("about:blank", ""));
    }

    #[test]
    fn a_search_for_the_entered_text_is_told_from_a_redirect() {
        // #865: the address bar searched for the text it was left with.
        assert!(searched_for(
            "p://192.168.1.211:8090/v1/blocks",
            "https://www.google.com/search?q=p%3A%2F%2F192.168.1.211%3A8090%2Fv1%2Fblocks&oq=p&sourceid=chrome"
        ));
        assert!(searched_for(
            "web+thing:a/b",
            "https://duckduckgo.com/?q=web%2Bthing%3Aa%2Fb"
        ));
        // A sign-in redirect carries the address too, and is not a search.
        assert!(!searched_for(
            "https://mail.example.test/inbox",
            "https://accounts.example.test/login?continue=https%3A%2F%2Fmail.example.test%2Finbox"
        ));
        assert!(!searched_for("p://host/x", "https://host.test/other"));
    }

    #[test]
    fn results_name_error_pages_tabs_and_long_urls() {
        assert!(is_error_page("chrome-error://chromewebdata/"));
        assert!(!is_error_page("https://example.test/chrome-error:"));
        assert_eq!(shown_url("https://example.test/"), "https://example.test/");
        let long = format!("data:text/html,{}", "x".repeat(5000));
        let shown = shown_url(&long);
        assert!(shown.starts_with("data:text/html,xxx") && shown.ends_with("[5015 characters]"));
        assert!(shown.chars().count() < 2040);
        assert!(caption_names_tab("New Tab - Google Chrome", "New Tab"));
        assert!(caption_names_tab("page - Audio playing - Google Chrome", "page"));
        assert!(caption_names_tab("page", "page"));
        // #866: a tab is not the window's tab because its title starts the caption.
        assert!(!caption_names_tab("page-alpha - Google Chrome", "page"));
        assert!(!caption_names_tab("New Tab - Google Chrome", ""));
    }
}
