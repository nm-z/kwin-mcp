use super::{
    GRANT_BYTES, Grant, Interface, Load, Path, PathTable, REPORT_BYTES, Registry, Report,
    SHARE_LINGER, Shares, choose, fair_share, first, nl80211, paths_to,
};
use std::net::IpAddr;
use std::time::{Duration, Instant};

const MEGABIT: u64 = 1_000_000;
/// A gigabit interface's budget.
const BUDGET: u64 = 900 * MEGABIT;

fn grant(shares: &Shares, id: u64) -> Grant {
    first(shares.grants(), |(stream, _)| *stream == id).map_or_else(Grant::default, |(_, grant)| grant)
}

/// The paces of the streams with a picture on the wire, added up.
fn paced(shares: &Shares, sending: &[u64]) -> u64 {
    sending.iter().map(|id| grant(shares, *id).pace).sum()
}

/// One picture of stream `id`: on the wire at `from` for `wire`.
fn picture(shares: &mut Shares, id: u64, from: Instant, wire: Duration) -> Instant {
    shares.report(id, Report::Wire(true), from);
    shares.report(id, Report::Wire(false), from + wire);
    from + wire
}

#[test]
fn grants_and_reports_survive_the_socket() {
    let granted = Grant {
        pace: 450 * MEGABIT,
        share: 300 * MEGABIT,
        streams: 3,
    };
    assert_eq!(Grant::decode(&granted.encode()), granted);
    assert_eq!(granted.encode().len(), GRANT_BYTES);
    for report in [
        Report::Wire(true),
        Report::Wire(false),
        Report::Cap(Some(432 * MEGABIT)),
        Report::Cap(None),
    ] {
        assert_eq!(Report::decode(&report.encode()), Some(report));
    }
    let mut unknown = [0u8; REPORT_BYTES];
    unknown[0] = 99;
    assert_eq!(Report::decode(&unknown), None);
}

#[test]
fn a_stream_alone_has_the_whole_budget() {
    let mut shares = Shares::new(BUDGET);
    shares.join(1);
    let whole = Grant {
        pace: BUDGET,
        share: BUDGET,
        streams: 1,
    };
    assert_eq!(grant(&shares, 1), whole);
    // Whatever it does: sending, streaming, still.
    let mut now = Instant::now();
    for _ in 0..5 {
        shares.report(1, Report::Wire(true), now);
        assert_eq!(grant(&shares, 1), whole);
        now = picture(&mut shares, 1, now, Duration::from_millis(55));
        assert_eq!(grant(&shares, 1), whole);
        now += Duration::from_millis(5);
    }
    shares.expire(now + SHARE_LINGER * 2);
    assert_eq!(grant(&shares, 1), whole);
}

#[test]
fn the_streams_on_the_wire_divide_the_budget_and_an_idle_one_gives_its_part_away() {
    let mut shares = Shares::new(BUDGET);
    for id in 1..=3 {
        shares.join(id);
    }
    let now = Instant::now();
    // Nobody sends: each would start with everything.
    assert_eq!(grant(&shares, 1).pace, BUDGET);
    // One sends: it has everything, and the others would start with half.
    shares.report(1, Report::Wire(true), now);
    assert_eq!(grant(&shares, 1).pace, BUDGET);
    assert_eq!(grant(&shares, 2).pace, BUDGET / 2);
    assert_eq!(paced(&shares, &[1]), BUDGET);
    // Two send: half each.
    shares.report(2, Report::Wire(true), now);
    assert_eq!(grant(&shares, 1).pace, BUDGET / 2);
    assert_eq!(grant(&shares, 2).pace, BUDGET / 2);
    assert_eq!(grant(&shares, 3).pace, BUDGET / 3);
    assert_eq!(paced(&shares, &[1, 2]), BUDGET);
    // Three send: a third each.
    shares.report(3, Report::Wire(true), now);
    assert_eq!(paced(&shares, &[1, 2, 3]), BUDGET);
    assert_eq!(grant(&shares, 2).pace, BUDGET / 3);
    // The second one's picture has left: the two still sending take over
    // its part at once.
    shares.report(2, Report::Wire(false), now + Duration::from_millis(40));
    assert_eq!(grant(&shares, 1).pace, BUDGET / 2);
    assert_eq!(grant(&shares, 3).pace, BUDGET / 2);
    assert_eq!(paced(&shares, &[1, 3]), BUDGET);
    // A stream that leaves gives everything back.
    shares.leave(3);
    assert_eq!(grant(&shares, 1).pace, BUDGET);
    assert_eq!(shares.grants().len(), 2);
}

#[test]
fn the_share_counts_the_streams_that_send_pictures_in_a_row() {
    let mut shares = Shares::new(BUDGET);
    shares.join(1);
    shares.join(2);
    let start = Instant::now();
    // The first scrolls: a picture every 60 ms.
    let mut now = start;
    for _ in 0..4 {
        now = picture(&mut shares, 1, now, Duration::from_millis(55)) + Duration::from_millis(5);
    }
    // The second shows one picture after a still screen (a click): while it
    // is on the wire the pace halves, and nobody's share changes.
    shares.report(2, Report::Wire(true), now);
    shares.report(1, Report::Wire(true), now);
    assert_eq!(grant(&shares, 1).pace, BUDGET / 2);
    assert_eq!(
        (grant(&shares, 1).share, grant(&shares, 1).streams),
        (BUDGET, 1)
    );
    now += Duration::from_millis(110);
    shares.report(2, Report::Wire(false), now);
    shares.report(1, Report::Wire(false), now);
    // A blinking cursor: its next picture comes after more than the linger,
    // while the first stream scrolls on.
    now += SHARE_LINGER + Duration::from_millis(250);
    shares.expire(now);
    now = picture(&mut shares, 1, now, Duration::from_millis(55)) + Duration::from_millis(5);
    shares.report(1, Report::Wire(true), now);
    now = picture(&mut shares, 2, now, Duration::from_millis(55));
    assert_eq!(grant(&shares, 1).share, BUDGET);
    assert_eq!(grant(&shares, 1).pace, BUDGET);
    // Now the second scrolls too: its next picture follows at once, and from
    // then on the two streams have half each.
    now += Duration::from_millis(5);
    shares.report(2, Report::Wire(true), now);
    for id in [1, 2] {
        assert_eq!(
            grant(&shares, id),
            Grant {
                pace: BUDGET / 2,
                share: BUDGET / 2,
                streams: 2
            }
        );
    }
    // Between two pictures of a scrolling stream the other one is paced to
    // the whole budget, and its share stays half: the mode does not follow
    // every gap.
    now += Duration::from_millis(110);
    shares.report(2, Report::Wire(false), now);
    assert_eq!(
        grant(&shares, 1),
        Grant {
            pace: BUDGET,
            share: BUDGET / 2,
            streams: 2
        }
    );
    // The linger has not passed: nothing expires, and it is due.
    let due = shares.expire(now + SHARE_LINGER / 2);
    assert_eq!(due, Some(now + SHARE_LINGER));
    assert_eq!(grant(&shares, 1).share, BUDGET / 2);
    // The second stream stays still: after the linger the first has the
    // whole budget as its share again.
    assert_eq!(shares.expire(now + SHARE_LINGER), None);
    assert_eq!(
        grant(&shares, 1),
        Grant {
            pace: BUDGET,
            share: BUDGET,
            streams: 1
        }
    );
    // And the still stream would start with half beside the scrolling one.
    assert_eq!(grant(&shares, 2).share, BUDGET / 2);
}

#[test]
fn a_stream_behind_a_slower_link_leaves_the_rest_to_the_others() {
    let mut shares = Shares::new(BUDGET);
    for id in 1..=3 {
        shares.join(id);
    }
    let now = Instant::now();
    // The third viewer sits behind a wireless link that carries 200 Mb/s.
    shares.report(3, Report::Cap(Some(200 * MEGABIT)), now);
    for id in 1..=3 {
        shares.report(id, Report::Wire(true), now);
    }
    assert_eq!(grant(&shares, 3).pace, 200 * MEGABIT);
    assert_eq!(grant(&shares, 1).pace, 350 * MEGABIT);
    assert_eq!(grant(&shares, 2).pace, 350 * MEGABIT);
    assert_eq!(paced(&shares, &[1, 2, 3]), BUDGET);
    // Alone it still gets no more than its link carries.
    shares.report(1, Report::Wire(false), now);
    shares.report(2, Report::Wire(false), now);
    assert_eq!(grant(&shares, 3).pace, 200 * MEGABIT);
    assert_eq!(grant(&shares, 1).pace, 700 * MEGABIT);
    // A cap above the fair part changes nothing.
    shares.report(3, Report::Cap(Some(800 * MEGABIT)), now);
    shares.report(1, Report::Wire(true), now);
    assert_eq!(grant(&shares, 3).pace, BUDGET / 2);
    assert_eq!(grant(&shares, 1).pace, BUDGET / 2);
    // The budget can change with the link.
    shares.set_budget(90 * MEGABIT);
    assert_eq!(grant(&shares, 1).pace, 45 * MEGABIT);
}

fn address(text: &str) -> IpAddr {
    text.parse().unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
}

fn interface(name: &str, wireless: bool, mbps: Option<u64>, addresses: &[(&str, u8)]) -> Interface {
    Interface {
        name: name.to_owned(),
        loopback: name == "lo",
        running: true,
        wireless,
        mbps,
        addresses: addresses
            .iter()
            .map(|(text, prefix)| (address(text), *prefix))
            .collect(),
    }
}

/// A machine with a wired and a wireless interface in the listener's subnet,
/// a bridge beside them, and an interface that is down.
fn two_paths(wireless_mbps: Option<u64>) -> Vec<Interface> {
    let mut down = interface("enp4s0", false, None, &[("192.168.1.9", 24)]);
    down.running = false;
    vec![
        interface("lo", false, None, &[("127.0.0.1", 8), ("::1", 128)]),
        interface("wlan0", true, wireless_mbps, &[("192.168.1.6", 24)]),
        interface(
            "enp10s0",
            false,
            Some(1000),
            &[("192.168.1.211", 24), ("fe80::1", 64), ("fd00::211", 64)],
        ),
        interface("br-0", false, Some(10_000), &[("172.18.0.1", 16)]),
        down,
    ]
}

fn names(paths: &[Path]) -> Vec<&str> {
    paths.iter().map(|path| path.interface.as_str()).collect()
}

fn load(interface: &str, peer: &str, sender_mbps: Option<u64>) -> Load {
    Load {
        interface: interface.to_owned(),
        peer: peer.to_owned(),
        sender_bits_per_second: sender_mbps.map(|mbps| mbps * MEGABIT),
    }
}

fn chosen<'a>(paths: &'a [Path], loads: &[Load], peer: &str) -> &'a str {
    choose(paths, loads, peer).map_or("none", |path| path.interface.as_str())
}

#[test]
fn the_paths_are_the_running_interfaces_in_the_peers_subnet_wired_first() {
    let table = two_paths(Some(864));
    let peer = address("192.168.1.20");
    let paths = paths_to(&table, peer, Some(address("192.168.1.211")));
    assert_eq!(names(&paths), ["enp10s0", "wlan0"]);
    assert_eq!(paths[0].source, address("192.168.1.211"));
    assert!(paths[0].routed && !paths[0].wireless);
    assert_eq!(paths[0].usable_bits_per_second(), 900 * MEGABIT);
    assert_eq!(paths[1].source, address("192.168.1.6"));
    assert!(!paths[1].routed && paths[1].wireless);
    // Half of what the wireless interface receives at.
    assert_eq!(paths[1].usable_bits_per_second(), 432 * MEGABIT);
    assert_eq!(paths[0].describe(), "enp10s0 (192.168.1.211, 1000 Mb/s)");
    assert_eq!(paths[1].describe(), "wlan0 (192.168.1.6, 864 Mb/s, wireless)");

    // The routing table may prefer the wireless one; wired still comes first.
    let paths = paths_to(&table, peer, Some(address("192.168.1.6")));
    assert_eq!(names(&paths), ["enp10s0", "wlan0"]);
    assert!(!paths[0].routed && paths[1].routed);

    // A peer on this machine has no path to choose.
    for own in ["127.0.0.1", "::1", "192.168.1.211", "172.18.0.1"] {
        assert_eq!(paths_to(&table, address(own), None), [], "{own}");
    }
    // A peer behind a gateway is reached over the routed interface only.
    let paths = paths_to(&table, address("10.1.2.3"), Some(address("192.168.1.6")));
    assert_eq!(names(&paths), ["wlan0"]);
    // An IPv6 peer: the address in its prefix, never the link-local one.
    let paths = paths_to(&table, address("fd00::20"), None);
    assert_eq!(names(&paths), ["enp10s0"]);
    assert_eq!(paths[0].source, address("fd00::211"));
    assert_eq!(paths_to(&table, address("fe80::20"), None), []);
    // A machine with one interface has one path.
    let wired_only = [interface("enp0s25", false, Some(1000), &[("192.168.1.20", 24)])];
    let paths = paths_to(&wired_only, address("192.168.1.211"), Some(address("192.168.1.20")));
    assert_eq!(names(&paths), ["enp0s25"]);
    // Two wired interfaces: the faster first.
    let two_wired = [
        interface("enp1s0", false, Some(1000), &[("192.168.1.30", 24)]),
        interface("enp2s0", false, Some(2500), &[("192.168.1.31", 24)]),
    ];
    assert_eq!(
        names(&paths_to(&two_wired, peer, None)),
        ["enp2s0", "enp1s0"]
    );
}

#[test]
fn the_first_stream_takes_the_wired_path_whatever_the_wireless_one_reports() {
    for wireless in [Some(864), Some(2400), Some(5000), None] {
        let table = two_paths(wireless);
        let paths = paths_to(&table, address("192.168.1.20"), Some(address("192.168.1.6")));
        assert_eq!(chosen(&paths, &[], "sentry:47913"), "enp10s0", "{wireless:?}");
    }
    // Without a wired path the wireless one carries it, speed or no speed.
    let wireless_only = [interface("wlan0", true, None, &[("192.168.1.6", 24)])];
    let paths = paths_to(&wireless_only, address("192.168.1.20"), None);
    assert_eq!(chosen(&paths, &[], "sentry:47913"), "wlan0");
    assert_eq!(chosen(&[], &[], "sentry:47913"), "none");
}

#[test]
fn a_second_path_is_used_only_when_it_gives_more_than_a_fair_share_of_the_first() {
    let peer = address("192.168.1.20");
    let routed = Some(address("192.168.1.211"));
    // 864 Mb/s wireless carries 432; a second stream on the wire has 450.
    let slow = paths_to(&two_paths(Some(864)), peer, routed);
    let one = [load("enp10s0", "sentry:47913", Some(900))];
    assert_eq!(chosen(&slow, &one, "archy:47913"), "enp10s0");
    // The third has 300 on the wire: the wireless path is worth it now.
    let two = [
        load("enp10s0", "sentry:47913", Some(900)),
        load("enp10s0", "archy:47913", Some(900)),
    ];
    assert_eq!(fair_share(&slow[0], &two, "benji:47913"), 300 * MEGABIT);
    assert_eq!(fair_share(&slow[1], &two, "benji:47913"), 432 * MEGABIT);
    assert_eq!(chosen(&slow, &two, "benji:47913"), "wlan0");
    // The fourth: 300 on the wire against 216 beside the wireless stream.
    let three = [two[0].clone(), two[1].clone(), load("wlan0", "benji:47913", Some(900))];
    assert_eq!(chosen(&slow, &three, "azb1:47913"), "enp10s0");

    // A faster wireless link (2400 Mb/s carries 1200) takes the second.
    let fast = paths_to(&two_paths(Some(2400)), peer, routed);
    assert_eq!(chosen(&fast, &one, "archy:47913"), "wlan0");
    // A wireless link that reports no speed is never a second path.
    let unknown = paths_to(&two_paths(None), peer, routed);
    assert_eq!(chosen(&unknown, &two, "benji:47913"), "enp10s0");
    // Equal shares stay on the wire: 900 Mb/s wireless carries 450.
    let equal = paths_to(&two_paths(Some(900)), peer, routed);
    assert_eq!(chosen(&equal, &one, "archy:47913"), "enp10s0");
}

#[test]
fn streams_from_one_machine_share_its_interface_on_any_path() {
    let fast = paths_to(
        &two_paths(Some(2400)),
        address("192.168.1.20"),
        Some(address("192.168.1.211")),
    );
    // sentry sends on one gigabit interface: its second stream has 450
    // whichever path it arrives on, so it stays on the wire.
    let one = [load("enp10s0", "sentry:47913", Some(900))];
    assert_eq!(fair_share(&fast[1], &one, "sentry:47913"), 450 * MEGABIT);
    assert_eq!(chosen(&fast, &one, "sentry:47913"), "enp10s0");
    // A stream from another machine is not held back by sentry's interface.
    assert_eq!(fair_share(&fast[1], &one, "archy:47913"), 1200 * MEGABIT);
    assert_eq!(chosen(&fast, &one, "archy:47913"), "wlan0");
    // Until the sender has told its budget, only this machine's paths count.
    let untold = [load("enp10s0", "sentry:47913", None)];
    assert_eq!(chosen(&fast, &untold, "sentry:47913"), "wlan0");
    // A sender with a faster interface than this machine's wired one.
    let wide = [load("enp10s0", "sentry:47913", Some(9000))];
    assert_eq!(chosen(&fast, &wide, "sentry:47913"), "wlan0");
}

#[test]
fn a_listed_table_reads_as_paths() -> Result<(), serde_json::Error> {
    let table: PathTable = serde_json::from_str(
        r#"{"registry":"/run/user/1000/test-streams","paths":[
            {"interface":"wired0","source":"127.0.0.1","mbps":1000},
            {"interface":"wifi0","source":"127.0.0.2","mbps":2400,"wireless":true}]}"#,
    )?;
    assert_eq!(names(&table.paths), ["wired0", "wifi0"]);
    assert_eq!(table.paths[1].source, address("127.0.0.2"));
    assert!(table.paths[1].wireless && !table.paths[1].routed);
    assert_eq!(
        table.registry.as_deref(),
        Some(std::path::Path::new("/run/user/1000/test-streams"))
    );
    assert_eq!(serde_json::from_str::<PathTable>(r#"{"paths":[]}"#)?, PathTable::default());
    Ok(())
}

#[test]
fn the_registry_counts_the_streams_of_running_viewers_only() -> std::io::Result<()> {
    let dir = std::env::temp_dir().join(format!("kwin-mcp-links-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let registry = Registry::new(Some(dir.clone()));
    let paths = paths_to(
        &two_paths(Some(2400)),
        address("192.168.1.20"),
        Some(address("192.168.1.211")),
    );
    // Nothing is open: the wired path, and this process is recorded on it.
    let own = registry.claim(&paths, "sentry:47913")?;
    assert_eq!(own.as_ref().map(|path| path.interface.as_str()), Some("enp10s0"));
    let recorded = dir.join(format!("{}.json", std::process::id()));
    assert!(std::fs::read_to_string(&recorded)?.contains("enp10s0"));
    // This process's own stream does not count against itself.
    let again = registry.claim(&paths, "sentry:47913")?;
    assert_eq!(again.map(|path| path.interface), own.map(|path| path.interface));
    registry.sender_budget("enp10s0", "sentry:47913", Some(900 * MEGABIT))?;
    assert!(std::fs::read_to_string(&recorded)?.contains("900000000"));
    // Another viewer that runs (this test's parent, with its start time) is
    // on the wired path: the next stream from another machine goes wireless.
    let parent = std::os::unix::process::parent_id();
    let started = super::process_start(parent).unwrap_or(0);
    std::fs::write(
        dir.join(format!("{parent}.json")),
        format!(
            r#"{{"pid":{parent},"started":{started},"interface":"enp10s0","peer":"sentry:47913","sender_bits_per_second":900000000}}"#
        ),
    )?;
    let next = registry.claim(&paths, "archy:47913")?;
    assert_eq!(next.map(|path| path.interface).as_deref(), Some("wlan0"));
    // A file of a process that is gone, or of a later process with the same
    // number, is not a stream, and is removed.
    for (name, pid, started) in [("gone", 0x3fff_fff0u32, 1u64), ("reused", parent, started + 1)] {
        let stale = dir.join(format!("{name}.json"));
        std::fs::write(
            &stale,
            format!(r#"{{"pid":{pid},"started":{started},"interface":"wlan0","peer":"benji:47913"}}"#),
        )?;
        registry.claim(&paths, "archy:47913")?;
        assert!(!stale.exists(), "{name}");
    }
    registry.release();
    assert!(!recorded.exists());
    std::fs::remove_dir_all(&dir)
}

/// One netlink attribute holding `payload`.
fn attribute(kind: u16, payload: &[u8]) -> Vec<u8> {
    nl80211::attribute(kind, payload)
}

#[test]
fn the_receive_rate_is_read_from_a_stations_nested_attributes() {
    // NL80211_ATTR_STA_INFO (21) > NL80211_STA_INFO_RX_BITRATE (14) >
    // NL80211_RATE_INFO_BITRATE32 (5), in units of 100 kb/s, among others.
    let rate = [
        attribute(1, &8648u16.to_ne_bytes()),
        attribute(5, &8648u32.to_ne_bytes()),
        attribute(13, &[2]),
    ]
    .concat();
    let info = [
        attribute(7, &[0xc3]),
        attribute(8, &attribute(5, &7206u32.to_ne_bytes())),
        // The kernel marks nested attributes with the top bit of the type.
        attribute(14 | 0x8000, &rate),
    ]
    .concat();
    let station = [
        attribute(3, &3u32.to_ne_bytes()),
        attribute(6, &[0x4c, 0x19, 0x5d, 0x13, 0xe5, 0xfe]),
        attribute(21 | 0x8000, &info),
    ]
    .concat();
    assert_eq!(nl80211::receive_rate(&station), Some(864));
    // Only the 16-bit rate, as older drivers report it.
    let narrow = attribute(21, &attribute(14, &attribute(1, &1300u16.to_ne_bytes())));
    assert_eq!(nl80211::receive_rate(&narrow), Some(130));
    // No receive rate, a cut attribute, nothing.
    let sending_only = attribute(21, &attribute(8, &attribute(5, &7206u32.to_ne_bytes())));
    assert_eq!(nl80211::receive_rate(&sending_only), None);
    assert_eq!(nl80211::receive_rate(&station[..station.len() - 3]), None);
    assert_eq!(nl80211::receive_rate(&[]), None);
    assert_eq!(nl80211::lookup(&[9, 0, 1, 0, 1, 2, 3, 4, 5, 0, 0, 0], 1), Some(&[1u8, 2, 3, 4, 5][..]));
    assert_eq!(nl80211::lookup(&[2, 0, 1, 0], 1), None);

    // A reply datagram: two station messages, then the end of the dump.
    let message = |kind: u16, body: &[u8]| {
        let length = u32::try_from(16 + 4 + body.len()).unwrap_or(0);
        let mut bytes = length.to_ne_bytes().to_vec();
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&[0; 10]);
        bytes.extend_from_slice(&[17, 1, 0, 0]);
        bytes.extend_from_slice(body);
        bytes
    };
    let datagram = [message(34, &narrow), message(34, &station), message(3, &[])].concat();
    let (bodies, ended) = nl80211::messages(&datagram);
    assert!(ended);
    assert_eq!(bodies.len(), 2);
    assert_eq!(nl80211::receive_rate(bodies[1]), Some(864));
    let (bodies, ended) = nl80211::messages(&datagram[..message(34, &narrow).len()]);
    assert!(!ended);
    assert_eq!(bodies, [&narrow[..]]);
}
