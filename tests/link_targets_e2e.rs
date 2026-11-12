#![allow(clippy::expect_used)]

//! Where a link points is part of what the element tools report (#947): the
//! element search gives each `role=link` element its absolute `href`, from the page
//! (CDP) and from AT-SPI; `accessibility_tree show_links=true` puts the same target
//! on the link's row and stays within its 30,000 characters by returning a level of
//! several hundred links a page of rows at a time; `browser_read` returns the
//! page's text, outer HTML or links through CDP, bounded, without a save dialog.
//!
//! The page is a results page of 300 cards served from 127.0.0.1, with relative,
//! absolute, scheme-relative, fragment-only, `javascript:` and `mailto:` targets, a
//! link without `href`, a `role=link` span, a target of 3,000 characters and two
//! links inside an iframe. One case runs Chrome with CDP (a `--user-data-dir` of
//! its own), one without, and one types over the name in Chrome's save dialog with
//! `keyboard_type replace=true`.
//!
//! The server binary is `KWIN_MCP_E2E_SERVER` when set, else the one cargo built.
//! `KWIN_MCP_E2E_EVIDENCE` names a directory that receives screenshots.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const CARDS: usize = 300;
const TREE_BUDGET: usize = 30_000;
const LONG_QUERY: usize = 3_000;

/// The path and query of card `n`, as the page writes it (relative).
fn card_target(n: usize) -> String {
    format!("/itm/{}?hash=item{:x}&pos={n}", 100_000 + n, n * 7919)
}

fn card_name(n: usize) -> String {
    format!("Listing {n} vintage widget lot Opens in a new window or tab")
}

fn page() -> String {
    let mut html = String::from(
        "<!doctype html><html><head><meta charset=utf-8><title>Link fixture</title></head><body>\
         <h1 id=top>Results</h1><nav><a href=\"#section-3\">Jump to section 3</a> \
         <a href=\"javascript:void(0)\">Script action</a> \
         <a href=\"mailto:someone@example.com?subject=Hi\">Mail the seller</a> \
         <a>No target</a> <span role=link tabindex=0>Scripted span link</span> \
         <a href=\"https://example.com/abs/path?x=1&amp;y=2\">Absolute elsewhere</a> \
         <a href=\"../up/one.html\">Parent relative</a> \
         <a href=\"//example.net/scheme-relative\">Scheme relative</a></nav>",
    );
    html.push_str(&format!(
        "<a href=\"/long?q={}&amp;end=1\">Long target</a>",
        "z".repeat(LONG_QUERY)
    ));
    html.push_str(
        "<form><input type=password name=pw value=\"hunter2-secret\" aria-label=\"Password\" autofocus \
         oninput=\"pwlen.textContent=this.value.length\"><output id=pwlen>untouched</output>\
         <input type=text name=q value=\"visible-default\" aria-label=\"Query\"></form>\
         <template><input type=password value=\"template-secret\"></template>\
         <script>document.cookie=\"sid=cookie-secret\";try{localStorage.setItem(\"k\",\"storage-secret\")}catch(e){};\
         var inlineToken=\"script-secret\";</script>\
         <script type=\"application/json\">{\"state\":\"json-secret\"}</script>\
         <script type=\"application/ld+json\">{\"name\":\"ld-public\"}</script>\
         <style>.card{margin:2px}</style><ul id=results>",
    );
    for n in 1..=CARDS {
        html.push_str(&format!(
            "<li class=card><a href=\"{}\">{}</a></li>\n",
            card_target(n).replace('&', "&amp;"),
            card_name(n)
        ));
    }
    html.push_str(
        "</ul><h2 id=section-3>Section 3</h2>\
         <iframe src=\"/sub/frame.html\" width=600 height=120 title=\"Inner frame\"></iframe></body></html>",
    );
    html
}

const FRAME: &str = "<!doctype html><title>frame</title><a href=\"inner/target.html\">Frame relative</a> \
                     <a href=\"https://example.org/frame-abs\">Frame absolute</a>";

struct Fixture {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let port = listener.local_addr().expect("fixture address").port();
        listener.set_nonblocking(true).expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread::spawn(move || Self::respond(stream));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }
        });
        Self {
            port,
            stop,
            worker: Some(worker),
        }
    }

    fn respond(mut stream: TcpStream) {
        let _ = stream.set_nonblocking(false);
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("HTTP timeout");
        let mut reader = BufReader::new(stream.try_clone().expect("HTTP reader"));
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                break;
            }
        }
        let (status, body) = match path.as_str() {
            "/" | "/index.html" => ("200 OK", page()),
            "/sub/frame.html" => ("200 OK", FRAME.to_owned()),
            _ => ("404 Not Found", "<!doctype html><title>absent</title>".to_owned()),
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Wire {
    child: Child,
    stdin: ChildStdin,
    replies: mpsc::Receiver<Value>,
    early: HashMap<u64, Value>,
    next_id: u64,
    session: String,
    workdir: PathBuf,
    home: PathBuf,
}

impl Wire {
    fn start(name: &str) -> Self {
        let home = std::env::temp_dir().join(format!("kwin-mcp-links-{name}-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
        let binary = std::env::var("KWIN_MCP_E2E_SERVER")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned());
        let mut child = Command::new(binary)
            .args(["--autoclean", "--no-viewer"])
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            // A tester's own CHROME_CONFIG_HOME names a profile outside the private
            // HOME, read-only in the session: Chrome would not start without a
            // --user-data-dir, which is how the case without CDP runs it.
            .env_remove("CHROME_CONFIG_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kwin-mcp");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = child.stdout.take().expect("server stdout");
        let (send, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(message) = serde_json::from_str::<Value>(&line)
                    && send.send(message).is_err()
                {
                    break;
                }
            }
        });
        let mut wire = Self {
            child,
            stdin,
            replies,
            early: HashMap::new(),
            next_id: 0,
            session: String::new(),
            workdir: PathBuf::new(),
            home,
        };
        let id = wire.send(
            "initialize",
            json!({"protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"link-targets-e2e","version":"1"}}),
        );
        wire.reply(id);
        writeln!(
            wire.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .expect("initialized");
        let started = wire.tool("session_start", json!({"width":1280, "height":800}));
        let content = &started["result"]["structuredContent"];
        wire.session = content["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session_start: {started}"))
            .to_owned();
        wire.workdir = PathBuf::from(content["host_shared_dir"].as_str().expect("workdir"));
        wire
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .expect("send request");
        self.stdin.flush().expect("flush request");
        id
    }

    fn reply(&mut self, id: u64) -> Value {
        if let Some(reply) = self.early.remove(&id) {
            return reply;
        }
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let reply = self
                .replies
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("no reply to request {id}: {error}"));
            match reply["id"].as_u64() {
                Some(got) if got == id => return reply,
                Some(got) => {
                    self.early.insert(got, reply);
                }
                None => {}
            }
        }
    }

    fn send_tool(&mut self, name: &str, mut arguments: Value) -> u64 {
        if !self.session.is_empty() {
            arguments["session_id"] = json!(self.session);
        }
        self.send("tools/call", json!({"name":name, "arguments":arguments}))
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.send_tool(name, arguments);
        let reply = self.reply(id);
        let shown: String = reply.to_string().chars().take(600).collect();
        eprintln!("{name}: {shown}");
        reply
    }

    /// The structured content of a call that must succeed.
    fn data(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.tool(name, arguments);
        assert!(reply["error"].is_null(), "{name}: {reply}");
        reply["result"]["structuredContent"].clone()
    }

    /// The `reason` of a call that must be refused.
    fn refused(&mut self, name: &str, arguments: Value) -> (String, String) {
        let reply = self.tool(name, arguments);
        let error = &reply["error"];
        assert!(!error.is_null(), "{name} was not refused: {reply}");
        (
            error["data"]["reason"].as_str().unwrap_or_default().to_owned(),
            error["message"].as_str().unwrap_or_default().to_owned(),
        )
    }

    fn launch(&mut self, command: String) -> Value {
        let launched = self.tool(
            "launch_app",
            json!({"command":command, "window_timeout_seconds":60}),
        );
        assert!(launched["error"].is_null(), "{launched}");
        launched["result"]["structuredContent"].clone()
    }

    fn window_titles(&mut self) -> Vec<String> {
        self.data("window_list", json!({}))["windows"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|window| window["title"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn keep(&mut self, name: &str) {
        let Some(directory) = std::env::var_os("KWIN_MCP_E2E_EVIDENCE").map(PathBuf::from) else {
            return;
        };
        thread::sleep(Duration::from_millis(500));
        let shot = self.tool("screenshot", json!({}));
        let path = shot["result"]["structuredContent"]["path"]
            .as_str()
            .unwrap_or_else(|| panic!("screenshot: {shot}"));
        std::fs::create_dir_all(&directory).expect("create the evidence directory");
        std::fs::copy(path, directory.join(format!("{name}.png"))).expect("copy evidence");
    }

    /// Search for links named `query` and wait until the page's are there.
    fn links(&mut self, query: &str, extra: Value) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let mut arguments = json!({"query":query, "role":"link", "timeout_seconds":30, "limit":200});
            for (key, value) in extra.as_object().into_iter().flatten() {
                arguments[key] = value.clone();
            }
            let found = self.data("find_ui_elements", arguments);
            let elements = found["elements"].as_array().cloned().unwrap_or_default();
            if !elements.is_empty() {
                return elements;
            }
            assert!(Instant::now() < deadline, "no link named {query}: {found}");
            thread::sleep(Duration::from_millis(500));
        }
    }

    /// Every row of `accessibility_tree role=link`, read a page of rows at a time,
    /// with the number of calls it took. Each reply must fit the tree's budget.
    fn link_rows(&mut self, show_links: bool, show_elements: bool) -> (Vec<String>, usize) {
        let mut rows = Vec::new();
        let mut offset = 0;
        let mut calls = 0;
        loop {
            let tree = self.data(
                "accessibility_tree",
                json!({"role":"link", "show_links":show_links, "show_elements":show_elements,
                    "offset":offset, "timeout_seconds":30}),
            );
            calls += 1;
            let text = tree["tree"].as_str().expect("the tree");
            assert!(tree["incomplete"].is_null(), "the tree was cut by time: {tree}");
            assert!(
                text.chars().count() <= TREE_BUDGET,
                "{} characters in one reply",
                text.chars().count()
            );
            assert_eq!(tree["chars"].as_u64(), u64::try_from(text.chars().count()).ok());
            rows.extend(
                text.lines()
                    .map(str::trim_start)
                    .filter(|line| line.starts_with("link\t"))
                    .map(str::to_owned),
            );
            let Some(next) = tree["next_offset"].as_u64() else {
                return (rows, calls);
            };
            assert!(text.lines().last().is_some_and(|line| line.ends_with(&format!("offset={next}"))), "{text}");
            let next = usize::try_from(next).expect("offset");
            assert!(next > offset, "the offset did not advance: {tree}");
            offset = next;
            assert!(calls < 40, "the tree never ended");
        }
    }

    /// The whole of what `browser_read` returns for these arguments, read
    /// `max_chars` at a time, with the number of calls.
    fn read_all(&mut self, mut arguments: Value, max_chars: usize) -> (String, usize) {
        let mut whole = String::new();
        let mut calls = 0;
        arguments["max_chars"] = json!(max_chars);
        loop {
            let read = self.data("browser_read", arguments.clone());
            calls += 1;
            let content = read["content"].as_str().expect("content");
            assert!(content.encode_utf16().count() <= max_chars + 1, "{read}");
            assert_eq!(read["chars"].as_u64(), u64::try_from(content.encode_utf16().count()).ok());
            assert_eq!(read["source"], "cdp");
            whole.push_str(content);
            let Some(next) = read["next_offset"].as_u64() else {
                assert_eq!(
                    read["total_chars"].as_u64(),
                    u64::try_from(whole.encode_utf16().count()).ok(),
                    "the pages do not add up to the page"
                );
                return (whole, calls);
            };
            arguments["offset"] = json!(next);
            assert!(calls < 200, "the page never ended");
        }
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        if !self.session.is_empty() {
            let id = self.send_tool("session_stop", json!({}));
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                match self.replies.recv_timeout(Duration::from_millis(200)) {
                    Ok(reply) if reply["id"].as_u64() == Some(id) => break,
                    Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// The `href=` a tree row ends in.
fn row_target(row: &str) -> Option<&str> {
    row.split('\t').find_map(|field| field.strip_prefix("href="))
}

fn row_name(row: &str) -> &str {
    row.split('\t').nth(1).unwrap_or_default()
}

/// name → href of the matches of one backend.
fn targets(elements: &[Value], backend: &str) -> BTreeMap<String, Option<String>> {
    elements
        .iter()
        .filter(|element| element["backend"] == backend)
        .map(|element| {
            (
                element["name"].as_str().unwrap_or_default().to_owned(),
                element["href"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// The targets the page gives outside its cards, as a browser resolves them.
fn special_targets(base: &str) -> Vec<(&'static str, Option<String>)> {
    vec![
        ("Jump to section 3", Some(format!("{base}/index.html#section-3"))),
        ("Script action", Some("javascript:void(0)".to_owned())),
        ("Mail the seller", Some("mailto:someone@example.com?subject=Hi".to_owned())),
        ("Scripted span link", None),
        ("Absolute elsewhere", Some("https://example.com/abs/path?x=1&y=2".to_owned())),
        ("Parent relative", Some(format!("{base}/up/one.html"))),
        ("Scheme relative", Some("http://example.net/scheme-relative".to_owned())),
        ("Long target", Some(format!("{base}/long?q={}&end=1", "z".repeat(LONG_QUERY)))),
    ]
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn with_cdp_both_backends_the_tree_and_the_page_read_agree_on_every_target() {
    let fixture = Fixture::start();
    let base = fixture.base();
    let mut wire = Wire::start("cdp");
    let launched = wire.launch(format!(
        "google-chrome-stable --user-data-dir={} {base}/index.html",
        wire.workdir.join("profile").display()
    ));
    assert!(launched["cdp_port"].as_u64().is_some(), "no CDP: {launched}");

    // The element search: every card link, from the page and from AT-SPI. Right
    // after the launch the CDP connection does not list the tab yet, and a search
    // then holds the AT-SPI matches only.
    let deadline = Instant::now() + Duration::from_secs(60);
    let found = loop {
        let found = wire.links("Listing 7 ", json!({}));
        if found.iter().any(|element| element["backend"] == "cdp") {
            break found;
        }
        assert!(Instant::now() < deadline, "no match from the page: {found:?}");
        thread::sleep(Duration::from_millis(500));
    };
    wire.keep("cdp-page");
    let expected: BTreeMap<String, Option<String>> = std::iter::once(7)
        .chain(70..=79)
        .map(|n| (card_name(n), Some(format!("{base}{}", card_target(n)))))
        .collect();
    assert_eq!(targets(&found, "cdp"), expected, "page targets");
    assert_eq!(targets(&found, "atspi"), expected, "AT-SPI targets");
    for (name, target) in special_targets(&base) {
        let found = wire.links(name, json!({"exact":true}));
        for backend in ["cdp", "atspi"] {
            assert_eq!(
                targets(&found, backend).get(name),
                Some(&target),
                "{backend} target of {name}: {found:?}"
            );
        }
    }
    // The page script names an <a> without href a link; neither backend invents a target.
    for element in wire.links("No target", json!({"exact":true})) {
        assert!(element["href"].is_null(), "{element}");
    }
    // The iframe is its own document: its links resolve against the frame's address.
    let framed = wire.links("Frame relative", json!({"exact":true}));
    assert_eq!(
        targets(&framed, "atspi").get("Frame relative"),
        Some(&Some(format!("{base}/sub/inner/target.html"))),
        "{framed:?}"
    );
    // compact keeps its shape: no href.
    let compact = wire.links("Listing 7 ", json!({"compact":true}));
    assert!(compact.iter().all(|element| element["href"].is_null()), "{compact:?}");
    assert!(compact.iter().all(|element| element["bounds"].is_object()), "{compact:?}");

    // The tree: without show_links no row carries a target.
    let (plain, plain_calls) = wire.link_rows(false, false);
    assert!(plain.iter().all(|row| row_target(row).is_none()), "{plain:?}");
    assert!(plain.len() >= CARDS, "{} link rows", plain.len());
    // With it every card's row ends in the target the search reported, a page of rows
    // at a time.
    let (rows, calls) = wire.link_rows(true, false);
    eprintln!("tree: {} rows in {plain_calls} call(s) without targets, {} rows in {calls} calls with", plain.len(), rows.len());
    assert_eq!(rows.len(), plain.len(), "the paged rows are the rows");
    assert!(calls > 1, "300 targets fit one reply?");
    let by_name: BTreeMap<&str, Option<&str>> =
        rows.iter().map(|row| (row_name(row), row_target(row))).collect();
    for n in 1..=CARDS {
        assert_eq!(
            by_name.get(card_name(n).as_str()),
            Some(&Some(format!("{base}{}", card_target(n)).as_str())),
            "card {n}"
        );
    }
    for (name, target) in special_targets(&base) {
        let shown = by_name.get(name).copied().flatten();
        match target {
            // A target past 1,000 characters keeps its first and last 500.
            Some(target) if target.len() > 1_000 => {
                let shown = shown.expect("the long target");
                let note = format!("…[{} more characters]…", target.len() - 1_000);
                assert_eq!(shown, format!("{}{note}{}", &target[..500], &target[target.len() - 500..]));
            }
            target => assert_eq!(shown, target.as_deref(), "tree target of {name}"),
        }
    }

    // browser_read format=links: the same targets, whole, in pages.
    let (links, link_calls) = wire.read_all(json!({"format":"links"}), TREE_BUDGET);
    assert!(link_calls > 1, "the links of the page fit one reply?");
    let read: BTreeMap<&str, &str> = links
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();
    let from_tree: BTreeSet<&str> = by_name
        .iter()
        .filter(|(name, _)| **name != "Long target")
        .filter_map(|(_, target)| *target)
        .collect();
    let from_page: BTreeSet<&str> = read
        .iter()
        .filter(|(name, _)| **name != "Long target")
        .map(|(_, target)| *target)
        .collect();
    assert_eq!(from_page, from_tree, "browser_read and the tree name different targets");
    assert_eq!(
        read.get("Long target").copied(),
        special_targets(&base).pop().and_then(|(_, target)| target).as_deref()
    );
    // A selector scopes the read, and small pages add up to the same content.
    let (cards, _) = wire.read_all(json!({"format":"links", "selector":"#results"}), TREE_BUDGET);
    assert_eq!(cards.lines().count(), CARDS);
    let (in_pieces, piece_calls) =
        wire.read_all(json!({"format":"links", "selector":"#results"}), 1_000);
    assert_eq!(in_pieces, cards);
    assert!(piece_calls > 20, "{piece_calls} calls");
    let each = wire.data("browser_read", json!({"format":"links", "selector":"li.card a", "max_chars":500}));
    assert_eq!(each["matched"], CARDS);

    // Text and HTML carry what the page shows, never what it keeps. The password
    // field has focus: type into it, and let the page say how long its value is now.
    wire.data(
        "mouse_click_element",
        json!({"query":"Password", "role":"textbox", "exact":true, "timeout_seconds":20}),
    );
    let typed = wire.tool("keyboard_type", json!({"text":"typed-secret", "speed":3}));
    assert!(typed["error"].is_null(), "{typed}");
    let length = wire.data("browser_read", json!({"selector":"#pwlen"}));
    assert_eq!(length["content"], "26", "the field holds its 14 characters and the 12 typed");
    let (text, _) = wire.read_all(json!({}), TREE_BUDGET);
    assert!(text.contains(&card_name(CARDS)) && text.contains("Section 3"), "{}", &text[..200]);
    let (html, html_calls) = wire.read_all(json!({"format":"html"}), TREE_BUDGET);
    assert!(html_calls > 1);
    assert!(html.contains("name=\"pw\"") && html.contains("value=\"visible-default\""));
    assert!(html.contains(&card_target(CARDS).replace('&', "&amp;")));
    assert!(html.contains("ld-public"), "ld+json metadata is page content");
    for kept in [
        "hunter2-secret", "typed-secret", "template-secret", "cookie-secret", "storage-secret",
        "script-secret", "json-secret",
    ] {
        assert!(!text.contains(kept), "the text has {kept}");
        assert!(!html.contains(kept), "the HTML has {kept}");
        assert!(!links.contains(kept), "the links have {kept}");
    }
    let form = wire.data("browser_read", json!({"format":"html", "selector":"input[type=password]"}));
    let field = form["content"].as_str().unwrap_or_default();
    assert!(field.contains("name=\"pw\"") && !field.contains("value="), "{field}");

    // Refusals name their reason.
    assert_eq!(wire.refused("browser_read", json!({"selector":"#absent"})).0, "no_match");
    assert_eq!(wire.refused("browser_read", json!({"selector":"li["})).0, "invalid_selector");
    assert_eq!(wire.refused("browser_read", json!({"max_chars":0})).0, "invalid_max_chars");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn without_cdp_accessibility_gives_the_targets_and_the_page_read_says_what_it_needs() {
    let fixture = Fixture::start();
    let base = fixture.base();
    let mut wire = Wire::start("atspi");
    // Chrome's default profile directory: no CDP.
    let launched = wire.launch(format!("google-chrome-stable {base}/index.html"));
    assert!(launched["cdp_port"].is_null(), "CDP connected: {launched}");

    let found = wire.links("Listing 7 ", json!({}));
    wire.keep("atspi-page");
    assert!(found.iter().all(|element| element["backend"] == "atspi"), "{found:?}");
    let expected: BTreeMap<String, Option<String>> = std::iter::once(7)
        .chain(70..=79)
        .map(|n| (card_name(n), Some(format!("{base}{}", card_target(n)))))
        .collect();
    assert_eq!(targets(&found, "atspi"), expected);
    for (name, target) in special_targets(&base) {
        let found = wire.links(name, json!({"exact":true}));
        assert_eq!(targets(&found, "atspi").get(name), Some(&target), "{name}: {found:?}");
    }
    let framed = wire.links("Frame absolute", json!({"exact":true}));
    assert_eq!(
        targets(&framed, "atspi").get("Frame absolute"),
        Some(&Some("https://example.org/frame-abs".to_owned()))
    );

    // The AT-SPI tree: all links of the page (show_elements keeps the ones scrolled
    // out of view), each row with states, bounds and target, in pages.
    let (rows, calls) = wire.link_rows(true, true);
    eprintln!("tree: {} rows in {calls} calls", rows.len());
    assert!(calls > 1);
    let by_name: BTreeMap<&str, Option<&str>> =
        rows.iter().map(|row| (row_name(row), row_target(row))).collect();
    for n in 1..=CARDS {
        assert_eq!(
            by_name.get(card_name(n).as_str()),
            Some(&Some(format!("{base}{}", card_target(n)).as_str())),
            "card {n}"
        );
    }
    assert_eq!(
        by_name.get("Frame relative").copied().flatten(),
        Some(format!("{base}/sub/inner/target.html").as_str())
    );
    let (plain, _) = wire.link_rows(false, true);
    assert_eq!(plain.len(), rows.len());
    assert!(plain.iter().all(|row| row_target(row).is_none()));

    let (reason, message) = wire.refused("browser_read", json!({"format":"links"}));
    assert_eq!(reason, "cdp_unavailable");
    assert!(message.contains("CDP") && message.contains("--user-data-dir"), "{message}");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn replace_types_over_the_name_in_chromes_save_dialog() {
    let fixture = Fixture::start();
    let base = fixture.base();
    let mut wire = Wire::start("save");
    wire.launch(format!("google-chrome-stable {base}/index.html"));
    wire.links("Listing 7 ", json!({}));
    wire.data("keyboard_key", json!({"key":"ctrl+s"}));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !wire.window_titles().iter().any(|title| title == "Save File") {
        assert!(Instant::now() < deadline, "the save dialog never opened");
        thread::sleep(Duration::from_millis(300));
    }
    thread::sleep(Duration::from_secs(1));
    wire.keep("save-dialog");
    // The dialog shows accessibility nothing and opens with only the stem of the
    // suggested name selected: typing without replace would keep ".html".
    let saved = wire.workdir.join("saved-947.htm");
    let typed = wire.tool(
        "keyboard_type",
        json!({"text":saved.to_str().expect("path"), "replace":true, "speed":3}),
    );
    assert!(typed["error"].is_null(), "replace=true was refused: {typed}");
    let delivery = &typed["result"]["structuredContent"]["delivery"];
    assert_eq!(delivery["status"], "unverified", "{typed}");
    assert!(delivery["warning"].as_str().unwrap_or_default().contains("ctrl+a"), "{typed}");
    wire.keep("save-dialog-typed");
    // Return does not accept Chrome's save dialog; the Save mnemonic does.
    wire.data("keyboard_key", json!({"key":"Return"}));
    thread::sleep(Duration::from_secs(1));
    let after_return = wire.window_titles();
    eprintln!("after Return: {after_return:?}, saved: {}", saved.exists());
    if after_return.iter().any(|title| title == "Save File") {
        wire.data("keyboard_key", json!({"key":"alt+s"}));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !saved.exists() {
        assert!(
            Instant::now() < deadline,
            "nothing was saved as {}: {:?}",
            saved.display(),
            std::fs::read_dir(&wire.workdir).map(|entries| entries.flatten().map(|entry| entry.file_name()).collect::<Vec<_>>())
        );
        thread::sleep(Duration::from_millis(300));
    }
    assert!(!wire.workdir.join("saved-947.htm.html").exists(), "the old extension stayed");
}
