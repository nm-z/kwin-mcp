use super::{
    CLIPBOARD_MESSAGE_BYTES, CLIPBOARD_TEXT_BYTES, ClipText, DSC_SOONER_PERCENT,
    ENCODE_REMEASURE_FRAMES, Estimate, HEADER_BYTES, Header, Interface, LINK_SAMPLE_BYTES,
    LINK_USABLE_PERCENT, Mode, ModeControl, Notice, Payload, RAW_BPP_X16, Request, SETTLE_AFTER,
    SWITCH_AFTER_FRAMES, Scaler, Sent, Settle, StreamError, UNKNOWN_LINK_MBPS, USABLE_FRAME_TIME,
    Workers, budget_of, fit_size, pace, pack_rgb, packed_len, unpack_rgb, unsent_bytes,
};
use crate::dsc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[test]
fn a_header_survives_the_wire_and_foreign_bytes_do_not_parse() {
    let header = Header {
        payload: Payload::Dsc,
        settle: false,
        width: 1280,
        height: 720,
        bpp_x16: 128,
        sequence: 7,
        captured_micros: 1_762_732_800_123_456,
        sender_micros: 31_000,
    };
    let mut message = header.encode().to_vec();
    message.extend_from_slice(&[9, 8, 7]);
    assert_eq!(Header::decode(&message), Some((header, &[9u8, 8, 7][..])));
    assert_eq!(Header::decode(&message[..HEADER_BYTES - 1]), None);
    message[0] = 0;
    assert_eq!(Header::decode(&message), None);
    let empty = Header { width: 0, ..header };
    assert_eq!(Header::decode(&empty.encode()), None);
    let raw = Header {
        payload: Payload::Raw,
        bpp_x16: RAW_BPP_X16,
        ..header
    };
    assert_eq!(raw.raw_len(), 1280 * 720 * 3);
}

#[test]
fn a_settle_picture_is_marked_in_the_byte_older_viewers_ignore() {
    let exact = Header {
        payload: Payload::Raw,
        settle: true,
        width: 640,
        height: 360,
        bpp_x16: RAW_BPP_X16,
        sequence: 3,
        captured_micros: 5,
        sender_micros: 6,
    };
    let bytes = exact.encode();
    assert_eq!(bytes[1], 1);
    assert_eq!(Header::decode(&bytes), Some((exact, &[][..])));
    let plain = Header {
        settle: false,
        ..exact
    };
    assert_eq!(plain.encode()[1], 0);
    // Every other byte is the same, so a viewer that does not know the mark
    // reads an ordinary raw picture.
    assert_eq!(bytes[2..], plain.encode()[2..]);
    assert_eq!(bytes[0], plain.encode()[0]);
}

#[test]
fn notices_are_tagged_json() -> Result<(), serde_json::Error> {
    let hello = Notice::Hello {
        width: 1920,
        height: 1080,
        title: "kwin-viewer s1".to_owned(),
        budget_bits_per_second: None,
        link: "lo, loopback, unlimited".to_owned(),
        clipboard_text_bytes: None,
    };
    let text = serde_json::to_string(&hello)?;
    assert!(text.contains("\"type\":\"hello\""), "{text}");
    // A sender without the clipboard bridge says nothing about it, as the
    // builds before the bridge did.
    assert!(!text.contains("clipboard"), "{text}");
    assert_eq!(serde_json::from_str::<Notice>(&text)?, hello);
    let title: Notice = serde_json::from_str("{\"type\":\"title\",\"title\":\"Enter the code\"}")?;
    assert_eq!(
        title,
        Notice::Title {
            title: "Enter the code".to_owned()
        }
    );
    // What the two ends say about the links: the sender the stream's share,
    // the viewer the speed of a path it chose.
    let share = Notice::Share {
        bits_per_second: 450_000_000,
        streams: 2,
    };
    let text = serde_json::to_string(&share)?;
    assert_eq!(
        text,
        "{\"type\":\"share\",\"bits_per_second\":450000000,\"streams\":2}"
    );
    assert_eq!(Notice::decode(&text), Ok(share));
    let path = Request::Path {
        bits_per_second: 432_000_000,
    };
    let text = serde_json::to_string(&path)?;
    assert_eq!(text, "{\"type\":\"path\",\"bits_per_second\":432000000}");
    assert_eq!(Request::decode(&text), Some(path));
    Ok(())
}

#[test]
fn clipboard_messages_are_tagged_json_that_other_builds_ignore() -> Result<(), serde_json::Error> {
    let limit = u64::try_from(CLIPBOARD_TEXT_BYTES).unwrap_or(u64::MAX);
    let hello = Notice::Hello {
        width: 1280,
        height: 720,
        title: "kwin-viewer s2".to_owned(),
        budget_bits_per_second: Some(900_000_000),
        link: "eth0, 1000 Mb/s".to_owned(),
        clipboard_text_bytes: Some(limit),
    };
    let text = serde_json::to_string(&hello)?;
    assert!(text.contains("\"clipboard_text_bytes\":1048576"), "{text}");
    assert_eq!(Notice::decode(&text), Ok(hello));

    let pasted = ClipText::new(
        "naïve — 日本語\n\ttab \"quoted\"".to_owned(),
        CLIPBOARD_TEXT_BYTES,
    );
    let paste = Request::ClipboardPaste {
        text: pasted.clone(),
        secret: true,
    };
    let text = serde_json::to_string(&paste)?;
    assert!(text.starts_with("{\"type\":\"clipboard_paste\""), "{text}");
    assert_eq!(Request::decode(&text), Some(paste));
    // No text empties the session's selection for the paste.
    assert_eq!(
        Request::decode("{\"type\":\"clipboard_paste\"}"),
        Some(Request::ClipboardPaste {
            text: None,
            secret: false
        })
    );
    let copy = Request::ClipboardCopy {
        id: 7,
        max_bytes: limit,
    };
    assert_eq!(Request::decode(&serde_json::to_string(&copy)?), Some(copy));
    let answer = Notice::Clipboard {
        id: 7,
        text: pasted,
    };
    assert_eq!(Notice::decode(&serde_json::to_string(&answer)?), Ok(answer));

    // A type from a newer build is ignored, not an error, in both directions.
    assert_eq!(
        Request::decode("{\"type\":\"clipboard_image\",\"png\":\"AAAA\"}"),
        Some(Request::Unknown)
    );
    assert_eq!(
        Notice::decode("{\"type\":\"num_lock\",\"enabled\":true}"),
        Ok(Notice::Unknown)
    );
    // Anything else that does not parse is refused whole.
    assert_eq!(Request::decode("{\"type\":\"clipboard_copy\"}"), None);
    assert_eq!(Request::decode("not json"), None);
    Ok(())
}

#[test]
fn clipboard_text_is_bounded_and_never_shown() -> Result<(), serde_json::Error> {
    let longest = "x".repeat(CLIPBOARD_TEXT_BYTES);
    let too_long = format!("{longest}y");
    assert!(ClipText::new(longest.clone(), CLIPBOARD_TEXT_BYTES).is_some());
    assert!(ClipText::new(too_long.clone(), CLIPBOARD_TEXT_BYTES).is_none());
    // A peer's own lower limit holds too.
    assert!(ClipText::new("12345".to_owned(), 4).is_none());

    // The longest text crosses; one byte more does not parse at the receiver.
    let carried = |text: &str| {
        Request::decode(&format!(
            "{{\"type\":\"clipboard_paste\",\"text\":{}}}",
            serde_json::to_string(text).unwrap_or_default()
        ))
    };
    assert!(matches!(
        carried(&longest),
        Some(Request::ClipboardPaste { text: Some(text), .. }) if text.as_str() == longest
    ));
    assert_eq!(carried(&too_long), None);
    let answer = format!(
        "{{\"type\":\"clipboard\",\"id\":1,\"text\":{}}}",
        serde_json::to_string(&too_long)?
    );
    assert!(Notice::decode(&answer).is_err());
    // The longest text in JSON's longest spelling still fits one message.
    let escaped = format!(
        "{{\"type\":\"clipboard_paste\",\"text\":\"{}\"}}",
        "\\u0001".repeat(CLIPBOARD_TEXT_BYTES)
    );
    assert!(escaped.len() <= CLIPBOARD_MESSAGE_BYTES);
    assert!(Request::decode(&escaped).is_some());

    // Neither the text nor a parser's complaint about it reaches a log.
    let secret = "hunter2-do-not-log";
    let text = ClipText::new(secret.to_owned(), CLIPBOARD_TEXT_BYTES);
    let shown = format!(
        "{:?} {:?}",
        Request::ClipboardPaste {
            text: text.clone(),
            secret: true
        },
        Notice::Clipboard { id: 1, text }
    );
    assert!(!shown.contains(secret), "{shown}");
    let wrong_type = format!("{{\"type\":\"clipboard\",\"id\":\"{secret}\"}}");
    let refused = Notice::decode(&wrong_type);
    assert!(
        refused
            .as_ref()
            .is_err_and(|reason| !reason.contains(secret)),
        "{refused:?}"
    );
    Ok(())
}

#[test]
fn pictures_are_never_larger_than_the_session() {
    assert_eq!(fit_size((1920, 1080), (1280, 720)), (1280, 720));
    assert_eq!(fit_size((1920, 1080), (3840, 2160)), (1920, 1080));
    assert_eq!(fit_size((1920, 1080), (2560, 700)), (1920, 700));
    assert_eq!(fit_size((1920, 1080), (0, 0)), (1, 1));
}

#[test]
fn the_budget_is_the_link_speed_unless_loopback_or_overridden() {
    let interface = |name: &str, loopback: bool, wireless: bool, mbps: Option<u64>| Interface {
        name: name.to_owned(),
        loopback,
        running: true,
        wireless,
        mbps,
        addresses: Vec::new(),
    };
    let wired = interface("enp10s0", false, false, Some(1000));
    let budget = budget_of(None, Some(&wired));
    assert_eq!(budget.bits_per_second, Some(900_000_000));
    assert_eq!(budget.interface, "enp10s0");
    let loopback = interface("lo", true, false, None);
    assert_eq!(budget_of(None, Some(&loopback)).bits_per_second, None);
    assert_eq!(
        budget_of(Some(100), Some(&loopback)).bits_per_second,
        Some(90_000_000)
    );
    // A down link reports nothing, and the rate a wireless one receives at
    // is not what it sends at.
    let assumed = Some(UNKNOWN_LINK_MBPS * 10_000 * LINK_USABLE_PERCENT);
    for unknown in [
        interface("enp3s0", false, false, None),
        interface("wlan0", false, true, None),
        interface("wlan0", false, true, Some(864)),
    ] {
        assert_eq!(budget_of(None, Some(&unknown)).bits_per_second, assumed);
    }
    let unknown = budget_of(None, None);
    assert_eq!(unknown.bits_per_second, assumed);
    assert_eq!(unknown.interface, "");
    assert!(
        budget_of(Some(250), Some(&wired))
            .link
            .contains("--link-mbps 250")
    );
}

#[test]
fn a_changed_share_changes_the_link_speed_the_rule_works_with_at_once() {
    let megabits = |control: &ModeControl| {
        control
            .estimate(PIXELS_1080P)
            .link_bits_per_second
            .map(|bits| bits / MEGABIT)
    };
    // Alone on a gigabit link: exact 1080p pictures, 56 ms each.
    let mut control = ModeControl::new(Some(900 * MEGABIT));
    control.encode_measured(PIXELS_1080P, millis(20));
    assert_eq!(control.mode(PIXELS_1080P, true), Mode::Raw);
    control.sent(&sent(Mode::Raw, PIXELS_1080P, millis(2), 880));
    assert_eq!(megabits(&control), Some(880));
    // A second viewer starts streaming: half the budget, and the measured
    // speed halves with it before any picture measured the new share.
    control.share(450 * MEGABIT);
    assert_eq!(megabits(&control), Some(440));
    // Raw now takes 115 ms, DSC at the carried rate far less: the mode
    // follows the share after the usual four pictures, not at once.
    for _ in 0..SWITCH_AFTER_FRAMES - 1 {
        assert_eq!(control.mode(PIXELS_1080P, true), Mode::Raw);
        control.sent(&sent(Mode::Raw, PIXELS_1080P, millis(2), 440));
    }
    assert!(!control.mode(PIXELS_1080P, true).is_raw());
    assert_eq!(control.switches(), 1);
    // The same share again changes nothing.
    control.share(450 * MEGABIT);
    assert_eq!(megabits(&control), Some(440));
    // The other viewer stops: the whole budget again, never more than it.
    control.share(900 * MEGABIT);
    assert_eq!(megabits(&control), Some(880));
    control.share(2000 * MEGABIT);
    assert_eq!(megabits(&control), Some(880));
    for _ in 0..SWITCH_AFTER_FRAMES {
        control.mode(PIXELS_1080P, true);
    }
    assert_eq!(control.mode(PIXELS_1080P, true), Mode::Raw);
    assert_eq!(control.switches(), 2);

    // A stream that starts beside another decides its first picture for its
    // share: 720p raw takes 49 ms on 450 Mb/s, 1080p raw 110 ms.
    let mut second = ModeControl::new(Some(900 * MEGABIT));
    second.share(450 * MEGABIT);
    assert_eq!(megabits(&second), Some(450));
    second.encode_measured(PIXELS_1080P, millis(20));
    assert!(!second.mode(PIXELS_1080P, true).is_raw());
    let mut alone = ModeControl::new(Some(900 * MEGABIT));
    alone.encode_measured(PIXELS_1080P, millis(20));
    assert_eq!(alone.mode(PIXELS_1080P, true), Mode::Raw);

    // An unlimited stream has no share.
    let mut unlimited = ModeControl::new(None);
    unlimited.share(10 * MEGABIT);
    assert_eq!(megabits(&unlimited), None);
}

const PIXELS_1080P: u64 = 1920 * 1080;
const PIXELS_720P: u64 = 1280 * 720;
const MEGABIT: u64 = 1_000_000;

fn millis(value: u64) -> Duration {
    Duration::from_millis(value)
}

/// Bits per pixel as the codec counts them.
fn bpp(whole: u16) -> Mode {
    Mode::Dsc {
        bpp_x16: whole * 16,
    }
}

/// A picture that crossed a link of `megabits` in `mode`, after `prepare` of
/// packing or encoding.
fn sent(mode: Mode, pixels: u64, prepare: Duration, megabits: u64) -> Sent {
    let bits = match mode {
        Mode::Raw => pixels * 24,
        Mode::Dsc { bpp_x16 } => pixels * u64::from(bpp_x16) / 16,
    };
    Sent {
        mode,
        pixels,
        prepare,
        bytes: usize::try_from(bits / 8).unwrap_or(usize::MAX),
        wire: Duration::from_nanos(bits * 1_000_000_000 / (megabits * MEGABIT)),
    }
}

/// Sends `pictures` pictures of `pixels` pixels over a link of `megabits`,
/// each in the mode the rule gives, and returns the modes.
fn run(
    control: &mut ModeControl,
    pixels: u64,
    megabits: u64,
    pack: Duration,
    encode: Duration,
    pictures: u32,
) -> Vec<Mode> {
    (0..pictures)
        .map(|_| {
            if control.wants_encode_time(pixels) {
                control.encode_measured(pixels, encode);
            }
            let mode = control.mode(pixels, true);
            let prepare = if mode.is_raw() { pack } else { encode };
            control.sent(&sent(mode, pixels, prepare, megabits));
            mode
        })
        .collect()
}

#[test]
fn a_gigabit_link_carries_exact_1080p_pictures_however_slow_the_encoder() {
    // The measured case: 6,220,800 bytes cross 900 Mb/s in 55 ms, and the
    // encoder needs 50 ms a picture.
    let mut control = ModeControl::new(Some(900 * MEGABIT));
    assert_eq!(
        control.estimate(PIXELS_1080P).raw,
        Duration::from_micros(55_296)
    );
    assert!(!control.wants_encode_time(PIXELS_1080P));
    let modes = run(&mut control, PIXELS_1080P, 900, millis(1), millis(50), 100);
    assert!(modes.iter().all(|mode| mode.is_raw()), "{modes:?}");
    assert_eq!(control.switches(), 0);
    // And at 1280x720: 24.6 ms a picture.
    let modes = run(&mut control, PIXELS_720P, 900, millis(1), millis(22), 100);
    assert!(modes.iter().all(|mode| mode.is_raw()), "{modes:?}");
    assert_eq!(control.switches(), 0);
}

#[test]
fn an_unlimited_link_carries_exact_pictures() {
    let mut control = ModeControl::new(None);
    assert_eq!(
        control.estimate(PIXELS_1080P),
        Estimate {
            link_bits_per_second: None,
            raw: Duration::ZERO,
            dsc: Duration::ZERO,
            dsc_bpp_x16: dsc::MAX_BPP_X16,
        }
    );
    let modes = run(
        &mut control,
        PIXELS_1080P,
        20_000,
        millis(1),
        millis(50),
        20,
    );
    assert!(modes.iter().all(|mode| mode.is_raw()), "{modes:?}");
}

#[test]
fn a_slow_link_gets_dsc_at_the_rate_it_carries_from_the_first_picture() {
    // 100 Mb/s: 90 usable. Raw 1080p would take 553 ms; in 100 ms the link
    // carries 4.3 bpp, so the rate is the codec's lowest, 6 (138 ms).
    let mut control = ModeControl::new(Some(90 * MEGABIT));
    assert!(control.wants_encode_time(PIXELS_1080P));
    let modes = run(&mut control, PIXELS_1080P, 90, millis(1), millis(50), 40);
    assert!(modes.iter().all(|mode| *mode == bpp(6)), "{modes:?}");
    // 200 Mb/s: 180 usable carries 8.68 bpp, so 8.
    let mut control = ModeControl::new(Some(180 * MEGABIT));
    let modes = run(&mut control, PIXELS_1080P, 180, millis(1), millis(50), 40);
    assert!(modes.iter().all(|mode| *mode == bpp(8)), "{modes:?}");
    // 300 Mb/s: 270 usable carries 13 bpp, so the codec's highest, 12: 92 ms
    // and 20 ms to encode against raw's 184 ms.
    let mut control = ModeControl::new(Some(270 * MEGABIT));
    let modes = run(&mut control, PIXELS_1080P, 270, millis(1), millis(20), 40);
    assert!(modes.iter().all(|mode| *mode == bpp(12)), "{modes:?}");
    assert_eq!(control.switches(), 0);
}

#[test]
fn a_slow_encoder_never_lowers_the_rate() {
    // 200 Mb/s carries 8 bpp whether a picture encodes in 5 ms or in 100 ms.
    for encode in [5, 50, 100] {
        let mut control = ModeControl::new(Some(180 * MEGABIT));
        let modes = run(
            &mut control,
            PIXELS_1080P,
            180,
            millis(1),
            millis(encode),
            40,
        );
        assert!(
            modes.iter().all(|mode| *mode == bpp(8)),
            "{encode} ms: {modes:?}"
        );
    }
    // An encoder too slow for DSC to be a quarter sooner than raw (276 ms)
    // means raw, not a lower rate: 92 ms on the wire and 150 ms to encode.
    let mut control = ModeControl::new(Some(180 * MEGABIT));
    let modes = run(&mut control, PIXELS_1080P, 180, millis(1), millis(150), 40);
    assert!(modes.iter().all(|mode| mode.is_raw()), "{modes:?}");
}

#[test]
fn dsc_is_used_only_when_it_is_a_quarter_sooner_than_raw() {
    // 500 Mb/s: 450 usable. Raw 1080p takes 110.6 ms (+1 ms to pack), DSC at
    // 12 bpp 55.3 ms on the wire.
    let raw = Duration::from_micros(110_592 + 1000);
    let limit = raw * u32::try_from(DSC_SOONER_PERCENT).unwrap_or(0) / 100;
    let wire = Duration::from_micros(55_296);
    // With 50 ms to encode DSC would arrive 5 ms sooner: not enough.
    let mut control = ModeControl::new(Some(450 * MEGABIT));
    let modes = run(&mut control, PIXELS_1080P, 450, millis(1), millis(50), 40);
    assert!(modes.iter().all(|mode| mode.is_raw()), "{modes:?}");
    assert!(wire + millis(50) > limit);
    // With 15 ms to encode it arrives in 70 ms instead of 112.
    let mut control = ModeControl::new(Some(450 * MEGABIT));
    let modes = run(&mut control, PIXELS_1080P, 450, millis(1), millis(15), 40);
    assert!(wire + millis(15) <= limit);
    // The first picture went out before any pack time was measured, when the
    // rule already held; the mode never changed after it.
    assert!(modes.iter().all(|mode| *mode == bpp(12)), "{modes:?}");
    assert_eq!(control.switches(), 0);
}

#[test]
fn a_mode_changes_after_four_pictures_in_a_row_and_does_not_flap() {
    // 1280x720 raw takes 100 ms at 221.2 Mb/s.
    let mut control = ModeControl::new(Some(900 * MEGABIT));
    let (pack, encode) = (Duration::ZERO, millis(10));
    control.encode_measured(PIXELS_720P, encode);
    assert!(
        run(&mut control, PIXELS_720P, 900, pack, encode, 8)
            .iter()
            .all(|mode| mode.is_raw())
    );
    // The mode of the next picture when the link measures `megabits`.
    let at = |control: &mut ModeControl, megabits: u64| {
        control.link = Some(megabits * MEGABIT);
        control.mode(PIXELS_720P, true)
    };
    // The link falls to 100 Mb/s: raw would take 221 ms. Three slow pictures
    // are not enough, and one fast picture starts the count again.
    for _ in 0..3 {
        assert!(at(&mut control, 100).is_raw());
    }
    assert!(at(&mut control, 900).is_raw());
    for _ in 0..3 {
        assert!(at(&mut control, 100).is_raw());
    }
    assert_eq!(control.switches(), 0);
    // The fourth in a row: DSC at the 10 bpp that 100 Mb/s carries.
    assert_eq!(at(&mut control, 100), bpp(10));
    assert_eq!(control.switches(), 1);
    // Back and forth around the limit of 100 ms changes nothing: DSC stays
    // until raw takes no more than 90 ms.
    for megabits in [215, 230, 215, 230, 240, 215, 240, 230, 240, 240] {
        assert!(!at(&mut control, megabits).is_raw(), "{megabits}");
    }
    assert_eq!(control.switches(), 1);
    // 250 Mb/s carries a raw picture in 88 ms: raw again after four.
    let modes: Vec<_> = (0..4).map(|_| at(&mut control, 250)).collect();
    assert!(
        modes[..3].iter().all(|mode| !mode.is_raw()) && modes[3].is_raw(),
        "{modes:?}"
    );
    assert_eq!(control.switches(), 2);
    // And raw stays while it takes no more than 100 ms (221.2 Mb/s).
    for megabits in [230, 222, 240, 222, 222, 222, 222, 222] {
        assert!(at(&mut control, megabits).is_raw(), "{megabits}");
    }
    assert_eq!(control.switches(), 2);
    // Alternating either side of the limit never makes four in a row.
    for megabits in [215, 215, 215, 225, 215, 215, 215, 225, 215, 225] {
        assert!(at(&mut control, megabits).is_raw(), "{megabits}");
    }
    assert_eq!(control.switches(), 2);
}

#[test]
fn dsc_gives_way_to_raw_when_it_is_no_longer_sooner() {
    let mut control = ModeControl::new(Some(180 * MEGABIT));
    let modes = run(&mut control, PIXELS_1080P, 180, millis(1), millis(20), 8);
    assert!(modes.iter().all(|mode| *mode == bpp(8)), "{modes:?}");
    // The machine gets busy: 92 ms on the wire and 200 ms to encode is later
    // than raw's 277 ms. The rate stays 8 until the mode changes.
    let modes = run(&mut control, PIXELS_1080P, 180, millis(1), millis(200), 24);
    let first_raw = modes.iter().position(|mode| mode.is_raw());
    assert!(
        first_raw.is_some_and(|at| at >= 4
            && modes[..at].iter().all(|mode| *mode == bpp(8))
            && modes[at..].iter().all(|mode| mode.is_raw())),
        "{modes:?}"
    );
    assert_eq!(control.switches(), 1);
}

#[test]
fn the_rate_follows_the_link_with_a_margin_between_six_and_twelve() {
    let mut control = ModeControl::new(Some(180 * MEGABIT));
    let encode = millis(20);
    let modes = run(&mut control, PIXELS_1080P, 180, millis(1), encode, 8);
    assert!(modes.iter().all(|mode| *mode == bpp(8)), "{modes:?}");
    // 190 Mb/s carries 9.16 bpp, but 95% of it only 8.7: the rate stays.
    control.link = Some(190 * MEGABIT);
    for _ in 0..3 * SWITCH_AFTER_FRAMES {
        assert_eq!(control.mode(PIXELS_1080P, true), bpp(8));
    }
    // 200 Mb/s carries 9 with the margin: after four pictures.
    control.link = Some(200 * MEGABIT);
    let modes: Vec<_> = (0..4).map(|_| control.mode(PIXELS_1080P, true)).collect();
    assert_eq!(modes, [bpp(8), bpp(8), bpp(8), bpp(9)]);
    // 187 Mb/s still carries 9 (9.02): no change back.
    control.link = Some(187 * MEGABIT);
    for _ in 0..3 * SWITCH_AFTER_FRAMES {
        assert_eq!(control.mode(PIXELS_1080P, true), bpp(9));
    }
    // 150 Mb/s carries 7.2.
    control.link = Some(150 * MEGABIT);
    let modes: Vec<_> = (0..4).map(|_| control.mode(PIXELS_1080P, true)).collect();
    assert_eq!(modes, [bpp(9), bpp(9), bpp(9), bpp(7)]);
    // 20 Mb/s carries less than 1 bpp: the codec's lowest rate.
    control.link = Some(20 * MEGABIT);
    let modes: Vec<_> = (0..4).map(|_| control.mode(PIXELS_1080P, true)).collect();
    assert_eq!(modes[3], bpp(6));
    assert_eq!(control.estimate(PIXELS_1080P).dsc_bpp_x16, dsc::MIN_BPP_X16);
    // 400 Mb/s carries 19 bpp: the codec's highest rate, and raw would still
    // take 124 ms.
    control.link = Some(400 * MEGABIT);
    let modes: Vec<_> = (0..4).map(|_| control.mode(PIXELS_1080P, true)).collect();
    assert_eq!(modes[3], bpp(12));
    assert_eq!(control.switches(), 0);
}

#[test]
fn the_encoder_is_timed_again_while_raw_stays_too_slow() {
    // 100 Mb/s: raw 1080p takes 553 ms. The one trial encode took 500 ms
    // because the machine was busy, so with 138 ms on the wire DSC looked no
    // sooner and the pictures went out raw.
    let mut control = ModeControl::new(Some(90 * MEGABIT));
    assert!(control.wants_encode_time(PIXELS_1080P));
    control.encode_measured(PIXELS_1080P, millis(500));
    let mut trials = 0;
    let mut modes = Vec::new();
    while modes.last().is_none_or(|mode: &Mode| mode.is_raw()) && modes.len() < 500 {
        if control.wants_encode_time(PIXELS_1080P) {
            // The encoder really takes 50 ms.
            control.encode_measured(PIXELS_1080P, millis(50));
            trials += 1;
            assert_eq!(
                modes.len() % usize::try_from(ENCODE_REMEASURE_FRAMES).unwrap_or(1),
                0
            );
        }
        let mode = control.mode(PIXELS_1080P, true);
        control.sent(&sent(mode, PIXELS_1080P, millis(1), 90));
        modes.push(mode);
    }
    // 500 ms, then 388, 303 and 240 ms after three more trials: with 240 ms
    // DSC takes 378 ms, a quarter sooner than raw, and four pictures later
    // the stream is DSC.
    assert_eq!(trials, 3);
    assert_eq!(modes.last(), Some(&bpp(6)));
    assert_eq!(modes.len(), 3 * 32 + 4);
    // DSC pictures time the encoder themselves.
    for _ in 0..3 * ENCODE_REMEASURE_FRAMES {
        assert!(!control.wants_encode_time(PIXELS_1080P));
        let mode = control.mode(PIXELS_1080P, true);
        control.sent(&sent(mode, PIXELS_1080P, millis(50), 90));
    }
    // A link that carries raw pictures never asks.
    let mut control = ModeControl::new(Some(900 * MEGABIT));
    for _ in 0..3 * ENCODE_REMEASURE_FRAMES {
        assert!(!control.wants_encode_time(PIXELS_1080P));
        let mode = control.mode(PIXELS_1080P, true);
        control.sent(&sent(mode, PIXELS_1080P, millis(1), 900));
    }
}

#[test]
fn a_new_size_decides_at_once() {
    // 250 Mb/s usable: 1080p raw takes 199 ms, 720p raw 88 ms.
    let mut control = ModeControl::new(Some(250 * MEGABIT));
    control.encode_measured(PIXELS_1080P, millis(20));
    assert_eq!(control.mode(PIXELS_1080P, true), bpp(12));
    assert_eq!(control.switches(), 0);
    assert_eq!(control.mode(PIXELS_720P, true), Mode::Raw);
    assert_eq!(control.switches(), 1);
    assert_eq!(control.mode(PIXELS_1080P, true), bpp(12));
    assert_eq!(control.switches(), 2);
    // A size the codec cannot cut into slices goes out raw.
    assert_eq!(control.mode(PIXELS_1080P - 1, false), Mode::Raw);
    assert_eq!(control.switches(), 3);
}

#[test]
fn the_link_speed_is_measured_from_large_pictures_and_never_exceeds_the_budget() {
    let mut control = ModeControl::new(Some(900 * MEGABIT));
    let megabits = |control: &ModeControl| {
        control
            .estimate(PIXELS_1080P)
            .link_bits_per_second
            .map(|bits| bits / MEGABIT)
    };
    assert_eq!(megabits(&control), Some(900));
    // The first measurement replaces the nominal speed.
    control.sent(&sent(Mode::Raw, PIXELS_1080P, millis(1), 400));
    assert_eq!(megabits(&control), Some(400));
    // Later ones move it a quarter of the way.
    control.sent(&sent(Mode::Raw, PIXELS_1080P, millis(1), 800));
    assert_eq!(megabits(&control), Some(500));
    control.sent(&sent(Mode::Raw, PIXELS_1080P, millis(1), 100));
    assert_eq!(megabits(&control), Some(400));
    // A message too small to time says nothing about the link.
    control.sent(&Sent {
        mode: Mode::Raw,
        pixels: 100,
        prepare: Duration::ZERO,
        bytes: LINK_SAMPLE_BYTES - 1,
        wire: Duration::from_secs(1),
    });
    assert_eq!(megabits(&control), Some(400));
    // A measurement above the budget counts as the budget.
    for _ in 0..40 {
        control.sent(&sent(Mode::Raw, PIXELS_1080P, millis(1), 50_000));
    }
    assert_eq!(megabits(&control), Some(899));
    // Without a budget the measurement stands.
    let mut control = ModeControl::new(None);
    control.sent(&sent(Mode::Raw, PIXELS_1080P, millis(1), 5000));
    assert_eq!(megabits(&control), Some(5000));
}

#[test]
fn pack_and_encode_times_scale_with_the_picture() {
    let mut control = ModeControl::new(Some(90 * MEGABIT));
    control.encode_measured(PIXELS_1080P, millis(45));
    control.sent(&sent(Mode::Raw, PIXELS_1080P, millis(9), 90));
    let large = control.estimate(PIXELS_1080P);
    let small = control.estimate(PIXELS_720P);
    // 720p has 4/9 of the pixels: 20 ms to encode, 4 ms to pack.
    let wire = |pixels: u64, bits: u64| Duration::from_nanos(pixels * bits * 1000 / 90);
    // Times are kept per pixel in whole picoseconds.
    let close = |got: Duration, wanted: Duration| got.abs_diff(wanted) < Duration::from_micros(5);
    assert!(close(large.raw, millis(9) + wire(PIXELS_1080P, 24)));
    assert!(close(small.raw, millis(4) + wire(PIXELS_720P, 24)));
    assert!(close(large.dsc, millis(45) + wire(PIXELS_1080P, 6)));
    assert!(close(small.dsc, millis(20) + wire(PIXELS_720P, 9)));
    assert_eq!((large.dsc_bpp_x16, small.dsc_bpp_x16), (96, 144));
    assert!(large.raw > USABLE_FRAME_TIME);
}

#[test]
fn a_still_dsc_picture_is_sent_again_exactly_after_the_settle_time() {
    let start = Instant::now();
    let mut settle = Settle::default();
    assert_eq!(settle.due_in(start), None, "nothing was sent");
    settle.sent(false, start);
    assert_eq!(
        settle.due_in(start + SETTLE_AFTER * 4),
        None,
        "raw is exact"
    );
    settle.sent(true, start);
    assert_eq!(settle.due_in(start), Some(SETTLE_AFTER));
    assert_eq!(
        settle.due_in(start + millis(100)),
        Some(SETTLE_AFTER - millis(100))
    );
    assert_eq!(settle.due_in(start + SETTLE_AFTER), Some(Duration::ZERO));
    // The screen changed and went out as DSC again: the wait starts again.
    let changed = start + millis(140);
    settle.sent(true, changed);
    assert_eq!(settle.due_in(start + SETTLE_AFTER), Some(millis(140)));
    assert_eq!(settle.due_in(changed + SETTLE_AFTER), Some(Duration::ZERO));
    // The exact picture went out: nothing more is due.
    settle.sent(false, changed + SETTLE_AFTER);
    assert_eq!(settle.due_in(changed + SETTLE_AFTER * 10), None);
}

#[test]
fn a_paced_socket_sends_at_its_speed_and_reports_what_has_not_left() -> std::io::Result<()> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let mut sender = std::net::TcpStream::connect(listener.local_addr()?)?;
    let (mut receiver, _) = listener.accept()?;
    let reader = std::thread::spawn(move || {
        let mut taken = 0usize;
        let mut buffer = vec![0u8; 1 << 16];
        while let Ok(read) = receiver.read(&mut buffer) {
            if read == 0 {
                break;
            }
            taken += read;
        }
        taken
    });
    assert_eq!(unsent_bytes(&sender)?, 0);
    // 40 Mb/s is 5 MB a second: 1 MB takes 200 ms.
    pace(&sender, 40_000_000)?;
    let message = vec![7u8; 1_000_000];
    let send = |sender: &mut std::net::TcpStream| -> std::io::Result<(bool, Duration)> {
        let started = Instant::now();
        sender.write_all(&message)?;
        let mut seen_unsent = false;
        while unsent_bytes(sender)? > 0 {
            seen_unsent = true;
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the bytes never left"
            );
            std::thread::sleep(Duration::from_micros(250));
        }
        Ok((seen_unsent, started.elapsed()))
    };
    // The kernel does not pace a connection's first ten segments.
    send(&mut sender)?;
    let (seen_unsent, took) = send(&mut sender)?;
    assert!(seen_unsent, "1 MB left at once: the socket is not paced");
    assert!(
        took > millis(150) && took < millis(600),
        "1 MB at 40 Mb/s took {took:?}"
    );
    drop(sender);
    assert_eq!(reader.join().ok(), Some(2 * message.len()));
    Ok(())
}

#[test]
fn workers_run_every_index_once_on_borrowed_data_batch_after_batch() -> Result<(), StreamError> {
    let mut workers = Workers::new(6);
    assert_eq!(workers.threads(), 6);
    for count in [0usize, 1, 2, 5, 6, 7, 40] {
        let calls: Vec<AtomicUsize> = (0..count).map(|_| AtomicUsize::new(0)).collect();
        let threads = std::sync::Mutex::new(std::collections::HashSet::new());
        workers.run(count, &|index| {
            calls[index].fetch_add(1, Ordering::Relaxed);
            if let Ok(mut threads) = threads.lock() {
                threads.insert(std::thread::current().id());
            }
            std::thread::sleep(Duration::from_millis(2));
        })?;
        assert!(
            calls.iter().all(|calls| calls.load(Ordering::Relaxed) == 1),
            "{count}"
        );
        let used = threads.lock().map_or(0, |threads| threads.len());
        assert!(used <= 6 && (count < 6 || used > 1), "{count}: {used}");
    }
    // One thread in all: the caller does the work.
    let mut alone = Workers::new(1);
    let total = AtomicUsize::new(0);
    alone.run(9, &|index| {
        total.fetch_add(index, Ordering::Relaxed);
    })?;
    assert_eq!(total.load(Ordering::Relaxed), 36);
    Ok(())
}

#[test]
fn a_panicking_job_is_reported_and_the_workers_carry_on() -> Result<(), StreamError> {
    let mut workers = Workers::new(4);
    let done = AtomicUsize::new(0);
    let outcome = workers.run(8, &|index| {
        assert!(index != 3, "job 3 fails");
        done.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(outcome, Err(StreamError::WorkerPanicked));
    assert_eq!(done.load(Ordering::Relaxed), 7);
    done.store(0, Ordering::Relaxed);
    workers.run(8, &|_| {
        done.fetch_add(1, Ordering::Relaxed);
    })?;
    assert_eq!(done.load(Ordering::Relaxed), 8);
    Ok(())
}

fn solid(width: usize, height: usize, stride: usize, pixel: [u8; 4]) -> Vec<u8> {
    let mut data = vec![0xEEu8; stride * height];
    for row in data.chunks_exact_mut(stride) {
        for target in row[..width * 4].chunks_exact_mut(4) {
            target.copy_from_slice(&pixel);
        }
    }
    data
}

fn scaled(
    scaler: &mut Scaler,
    workers: &mut Workers,
    source: &[u8],
    stride: usize,
) -> Result<Vec<u8>, StreamError> {
    let mut out = vec![0xAAu8; scaler.scaled_len()];
    scaler.scale(workers, source, stride, &mut out)?;
    Ok(out)
}

#[test]
fn scaling_averages_the_area_each_output_pixel_covers() -> Result<(), StreamError> {
    let mut workers = Workers::new(1);
    // 4x2 to 2x1: each output pixel is the mean of a 2x2 block.
    let source: Vec<u8> = [
        [0u8, 0, 0, 0],
        [100, 40, 8, 0],
        [200, 200, 200, 0],
        [200, 200, 200, 0],
        [0, 0, 0, 0],
        [100, 40, 8, 0],
        [0, 0, 0, 0],
        [0, 0, 0, 0],
    ]
    .concat();
    let out = scaled(&mut Scaler::new((4, 2), (2, 1))?, &mut workers, &source, 16)?;
    assert_eq!(out, [50, 20, 4, 0, 100, 100, 100, 0]);
    // 3 to 2 columns: weights 2/3 and 1/3.
    let source: Vec<u8> = [[0u8, 0, 0, 0], [90, 90, 90, 0], [30, 30, 30, 0]].concat();
    let out = scaled(&mut Scaler::new((3, 1), (2, 1))?, &mut workers, &source, 12)?;
    assert_eq!(out, [30, 30, 30, 0, 50, 50, 50, 0]);
    Ok(())
}

#[test]
fn scaling_keeps_flat_colour_exact_and_ignores_row_padding_and_threads() -> Result<(), StreamError>
{
    let pixel = [17, 130, 255, 255];
    let stride = 1920 * 4 + 64;
    let source = solid(1920, 1080, stride, pixel);
    let (mut alone, mut seven) = (Workers::new(1), Workers::new(7));
    for target in [(1280, 720), (1277, 713), (1920, 540), (1, 1), (1920, 1080)] {
        let mut scaler = Scaler::new((1920, 1080), target)?;
        let one = scaled(&mut scaler, &mut alone, &source, stride)?;
        let many = scaled(&mut scaler, &mut seven, &source, stride)?;
        assert_eq!(
            one.len(),
            usize::try_from(target.0 * target.1 * 4).unwrap_or(0)
        );
        assert!(
            one.chunks_exact(4).all(|scaled| scaled == pixel),
            "{target:?}"
        );
        assert_eq!(one, many, "{target:?}");
    }
    Ok(())
}

#[test]
fn scaling_a_gradient_matches_single_threaded_output_and_never_upscales() -> Result<(), StreamError>
{
    let (width, height) = (640usize, 360usize);
    let mut source = vec![0u8; width * height * 4];
    for (index, byte) in source.iter_mut().enumerate() {
        *byte = u8::try_from((index * 31 + index / 7) % 251).unwrap_or(0);
    }
    let mut scaler = Scaler::new((640, 360), (417, 233))?;
    let one = scaled(&mut scaler, &mut Workers::new(1), &source, width * 4)?;
    let mut five = Workers::new(5);
    // The same scaler and workers picture after picture.
    for _ in 0..3 {
        assert_eq!(scaled(&mut scaler, &mut five, &source, width * 4)?, one);
    }
    assert!(Scaler::new((640, 360), (641, 360)).is_err());
    assert!(Scaler::new((640, 360), (0, 10)).is_err());
    let mut out = vec![0u8; scaler.scaled_len()];
    assert_eq!(
        scaler.scale(&mut five, &source[..source.len() - 1], width * 4, &mut out),
        Err(StreamError::FrameTooSmall)
    );
    assert_eq!(
        scaler.scale(&mut five, &source, width * 4, &mut out[1..]),
        Err(StreamError::OutputSize)
    );
    Ok(())
}

#[test]
fn raw_pixels_drop_the_padding_byte_and_come_back_as_rgba() -> Result<(), StreamError> {
    let (width, height, stride) = (5usize, 3usize, 5 * 4 + 8);
    let mut source = vec![0xEEu8; stride * height];
    let mut expected = Vec::new();
    for (row, line) in source.chunks_exact_mut(stride).enumerate() {
        for (column, pixel) in line[..width * 4].chunks_exact_mut(4).enumerate() {
            let value = u8::try_from(row * 16 + column).unwrap_or(0);
            // Blue, green, red, padding: the layout KWin delivers.
            pixel.copy_from_slice(&[value, value + 100, value + 200, 0]);
            expected.extend_from_slice(&[value + 200, value + 100, value, 255]);
        }
    }
    assert_eq!(packed_len((5, 3)), width * height * 3);
    for threads in [1, 4] {
        let mut workers = Workers::new(threads);
        let mut wire = vec![0u8; packed_len((5, 3))];
        pack_rgb(&mut workers, &source, stride, (5, 3), &mut wire)?;
        let mut rgba = vec![0u8; width * height * 4];
        unpack_rgb(&mut workers, &wire, width, &mut rgba)?;
        assert_eq!(rgba, expected);
        assert_eq!(
            unpack_rgb(&mut workers, &wire, width, &mut rgba[4..]),
            Err(StreamError::OutputSize)
        );
        assert_eq!(
            pack_rgb(&mut workers, &source, stride, (5, 3), &mut wire[3..]),
            Err(StreamError::OutputSize)
        );
        assert_eq!(
            pack_rgb(&mut workers, &source[..10], stride, (5, 3), &mut wire),
            Err(StreamError::FrameTooSmall)
        );
    }
    Ok(())
}

#[test]
fn every_colour_value_survives_pack_and_unpack_in_each_channel() -> Result<(), StreamError> {
    // 256 x 3 pixels: row 0 ramps red, row 1 green, row 2 blue, the other
    // channels held at values that would show a swap or a transfer curve.
    let (width, height) = (256usize, 3usize);
    let mut bgrx = Vec::with_capacity(width * height * 4);
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        for value in 0..=255u8 {
            let mut rgb = [11u8, 127, 242];
            rgb[row] = value;
            bgrx.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 0]);
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    let mut workers = Workers::new(3);
    let mut wire = vec![0u8; packed_len((256, 3))];
    pack_rgb(&mut workers, &bgrx, width * 4, (256, 3), &mut wire)?;
    let mut shown = vec![0u8; width * height * 4];
    unpack_rgb(&mut workers, &wire, width, &mut shown)?;
    assert_eq!(shown, rgba);
    Ok(())
}

#[test]
#[ignore = "measurement: prints the time to scale and pack a 1920x1080 picture"]
fn measure_scale_and_pack() -> Result<(), StreamError> {
    let (width, height) = (1920usize, 1080usize);
    let mut source = vec![0u8; width * height * 4];
    for (index, byte) in source.iter_mut().enumerate() {
        *byte = u8::try_from((index * 31 + index / 7) % 251).unwrap_or(0);
    }
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    const ROUNDS: u32 = 300;
    for threads in [1, cores] {
        let mut workers = Workers::new(threads);
        let mut scaler = Scaler::new((1920, 1080), (1280, 720))?;
        let mut out = vec![0u8; scaler.scaled_len()];
        let started = Instant::now();
        for _ in 0..ROUNDS {
            scaler.scale(&mut workers, &source, width * 4, &mut out)?;
        }
        let scale = started.elapsed() / ROUNDS;
        let mut message = vec![0u8; packed_len((1920, 1080))];
        let started = Instant::now();
        for _ in 0..ROUNDS {
            pack_rgb(&mut workers, &source, width * 4, (1920, 1080), &mut message)?;
            std::hint::black_box(&message);
        }
        let pack = started.elapsed() / ROUNDS;
        let mut rgba = vec![0u8; width * height * 4];
        let started = Instant::now();
        for _ in 0..ROUNDS {
            unpack_rgb(&mut workers, &message, width, &mut rgba)?;
            std::hint::black_box(&rgba);
        }
        let unpack = started.elapsed() / ROUNDS;
        eprintln!(
            "threads={threads}: scale 1920x1080 to 1280x720 {scale:?}, pack 1920x1080 {pack:?}, unpack 1920x1080 {unpack:?}"
        );
    }
    Ok(())
}
