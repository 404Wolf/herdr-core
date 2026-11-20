use super::*;

#[test]
fn continuation_restores_split_sequences_and_long_strings_resume_at_ground() {
    for prefix in [b"\x1b_Gm=1;AAAA".as_slice(), b"\x1b]2;title", b"\x1bP+q544", b"\x1b[31", b"\xf0\x9f"] {
        let mut screen = Terminal::new(80, 24, 1 << 20);
        screen.write(prefix);
        assert_eq!(screen.continuation().unwrap(), prefix);
    }
    let mut screen = Terminal::new(80, 24, 1 << 20);
    screen.write(b"\x1b_Gm=1;");
    screen.write(&vec![b'A'; CONTINUATION_MAX_BYTES]);
    assert!(screen.continuation().is_none());
    assert_eq!(screen.write_until_ground(b"AAAA"), None);
    assert_eq!(screen.write_until_ground(b"AA\x1b\\\x1b_Gm=1;BBBB"), Some(4));
    assert_eq!(screen.continuation(), Some(Vec::new()));
}

#[test]
fn display_changes_report_to_subscribers_and_queries_use_the_latest_facts() {
    let mut t = Terminal::new(80, 24, 0);
    t.write(b"\x1b[?2031h");
    t.take_responses(); // Enabling reporting may itself report the current scheme.
    t.set_display(Some([1, 2, 3]), Some([255; 3]), Some([4, 5, 6]), Some((9, 18)));
    assert_eq!(t.take_responses(), b"\x1b[?997;2n", "dark to light");
    t.set_display(None, Some([0; 3]), None, None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "light to dark");

    // Mode 2031 also tells subscribers to re-query colors within the same scheme.
    t.set_display(Some([7, 8, 9]), None, None, None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "foreground changed");
    t.set_display(None, None, Some([10, 11, 12]), None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "cursor changed");
    t.set_display(Some([7, 8, 9]), Some([0; 3]), Some([10, 11, 12]), Some((10, 20)));
    t.set_display(None, None, None, None);
    assert!(t.take_responses().is_empty(), "repeated colors and cell size are silent");

    t.write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07\x1b[?996n\x1b[16t");
    assert_eq!(
        t.take_responses(),
        b"\x1b]10;rgb:0707/0808/0909\x07\x1b]11;rgb:0000/0000/0000\x07\x1b]12;rgb:0a0a/0b0b/0c0c\x07\x1b[?997;1n\x1b[6;20;10t"
    );
}

#[test]
fn display_changes_do_not_report_without_a_subscription() {
    let mut t = Terminal::new(80, 24, 0);
    t.set_display(None, Some([255; 3]), None, None);
    assert!(t.take_responses().is_empty());
    t.write(b"\x1b[?2031h\x1b[?2031l");
    t.take_responses();
    t.set_display(None, Some([0; 3]), None, None);
    assert!(t.take_responses().is_empty(), "disabled subscription stays silent");
}

#[test]
fn a_repaint_into_a_dirty_viewer_turns_off_modes_the_screen_no_longer_has() {
    let mut viewer = Terminal::new(40, 10, 0);
    viewer.write(b"\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1004h");
    let screen = Terminal::new(40, 10, 0);
    viewer.write(&screen.paint(0));
    for mode in [1000, 1006, 2004, 1004] {
        assert!(!viewer.dec_mode(mode), "mode {mode} left on");
    }
    let mut mouse = Terminal::new(40, 10, 0);
    mouse.write(b"\x1b[?1000h");
    viewer.write(&mouse.paint(0));
    assert!(viewer.dec_mode(1000), "and the screen's own modes come back");
}

#[test]
fn parses_reads_and_round_trips_a_snapshot() {
    let mut t = Terminal::new(20, 3, 1 << 20);
    for i in 0..10 {
        t.write(format!("line {i}\r\n").as_bytes());
    }
    let text = t.plain_text();
    assert!(text.contains("line 0") && text.contains("line 9"), "{text:?}");

    // Snapshot taken in the middle of an escape sequence finishes it after restore.
    t.write(b"\x1b[3");
    let snap = t.snapshot().unwrap();
    let mut back = Terminal::restore(&snap, 1 << 20).unwrap();
    back.write(b"1mred\x1b[0m");
    assert_eq!(back.size(), (20, 3));
    let text = back.plain_text();
    assert!(text.contains("line 0") && text.contains("red"), "{text:?}");
    assert!(!text.contains("1mred"), "{text:?}");

    t.write(b"m\x1b[?1049h");
    assert!(t.alt_screen());
    assert!(Terminal::restore(b"garbage", 1 << 20).is_none());
}

#[test]
fn a_painted_terminal_looks_the_same_on_a_fresh_one() {
    let mut t = Terminal::new(30, 4, 1 << 20);
    for i in 0..8 {
        t.write(format!("\x1b[31mline {i}\x1b[0m\r\n").as_bytes());
    }
    t.write(b"\x1b]2;my title\x07\x1b[?2026h\x1b[?1004hprompt> ");
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&t.paint(3));
    let screen = |t: &Terminal| t.plain_text().lines().rev().take(4).map(String::from).collect::<Vec<_>>();
    assert_eq!(screen(&fresh), screen(&t));
    assert!(fresh.plain_text().contains("line 2"), "{:?}", fresh.plain_text());
    assert!(!fresh.plain_text().contains("line 1\n"), "history beyond 3 lines");
    assert_eq!(fresh.title(), "my title");
    assert!(fresh.dec_mode(1004));
    assert!(!fresh.dec_mode(2026), "synchronized output must not be replayed");

    let mut alt = Terminal::new(30, 4, 1 << 20);
    alt.write(b"main\r\n\x1b[?1049h\x1b[2;3Hfull screen");
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&alt.paint(10));
    assert!(fresh.alt_screen());
    assert!(fresh.plain_text().contains("full screen"));
}

#[test]
fn painted_history_reflows_soft_wraps_and_preserves_hard_lines_and_colors() {
    let input = b"\x1b[31mabcdefghij\x1b[32mklmnopqrstuvwx\x1b[44m  \x1b[0m\r\n\
hard one\r\nhard two\r\nscreen 1\r\nscreen 2\r\nprompt> ";
    let mut t = Terminal::new(10, 3, 1 << 20);
    t.write(input);
    let mut viewer = Terminal::new(10, 3, 1 << 20);
    viewer.write(&t.paint(100));
    assert_eq!(viewer.screen_rows(), t.screen_rows());
    assert!(viewer.plain_text().starts_with("abcdefghijklmnopqrstuvwx\nhard one\nhard two\n"));

    // The replayed history must reflow just as if the viewer had received it live.
    viewer.resize(30, 3);
    let mut wide = Terminal::new(30, 3, 1 << 20);
    wide.write(input);
    let history = |t: &Terminal| {
        let sel = t.select(ffi::POINT_HISTORY, (0, 0), (29, 2)).unwrap();
        t.format(ffi::FORMAT_VT, Some(&sel), false)
    };
    // Compare physical rows with styles, including the trailing colored blanks.
    assert_eq!(history(&viewer), history(&wide));
}

#[test]
fn repaint_uses_the_active_screen_not_stale_alternate_screen_modes() {
    let mut t = Terminal::new(30, 4, 1 << 20);
    t.write(b"main\r\n\x1b[?47h\x1b[?1049hALT\x1b[?1049lDONE");
    assert!(!t.alt_screen());
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&t.paint(10));
    assert!(!fresh.alt_screen());
    assert_eq!(fresh.screen_rows(), t.screen_rows());

    for enter in [47, 1047, 1049] {
        for exit in [None, Some(47), Some(1047), Some(1049)] {
            let mut t = Terminal::new(30, 4, 1 << 20);
            t.write(format!("main\r\n\x1b[?{enter}h\x1b[2;3HALT").as_bytes());
            if let Some(exit) = exit {
                t.write(format!("\x1b[?{exit}lDONE").as_bytes());
            }
            let paint = t.paint(10);
            for viewer_mode in [None, Some(47), Some(1047), Some(1049)] {
                let mut viewer = Terminal::new(30, 4, 1 << 20);
                if let Some(mode) = viewer_mode {
                    viewer.write(format!("old main\x1b[?{mode}hOLD ALT").as_bytes());
                }
                viewer.write(&paint);
                let case = (enter, exit, viewer_mode);
                assert_eq!(viewer.alt_screen(), t.alt_screen(), "{case:?}");
                assert_eq!(viewer.screen_rows(), t.screen_rows(), "{case:?}");
                assert!(!viewer.dec_mode(47), "{case:?}");
                assert!(!viewer.dec_mode(1047), "{case:?}");
                assert_eq!(viewer.dec_mode(1049), t.alt_screen(), "{case:?}");
            }
        }
    }
}

#[test]
fn program_status_reports_and_title_changes_reach_us() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.write(b"\x1b]7501;state=working:app=claude-code:progress=40\x1b\\");
    t.write(b"\x1b]7501;state=blocked:kind=permission:id=a/b:msg=QXBwbHk/\x07");
    t.write(b"\x1b]7501;state=clear\x1b\\\x1b]7501;state=bogus\x1b\\");
    let reports = t.take_status_reports();
    assert_eq!(reports.len(), 3, "{reports:?}");
    assert_eq!((reports[0].state, reports[0].progress, reports[0].app.as_str()), ("working", Some(40), "claude-code"));
    assert_eq!(
        (reports[1].kind, reports[1].id.as_str(), reports[1].message.as_str()),
        (Some("permission"), "a/b", "Apply?")
    );
    assert_eq!(reports[2].state, "clear");
    // A program asking whether we support the protocol gets an answer.
    t.write(b"\x1b]7501;?\x1b\\");
    assert!(String::from_utf8_lossy(&t.take_responses()).starts_with("\x1b]7501;?"));

    assert!(!t.take_title_changed());
    t.write(b"\x1b]2;hi\x07");
    assert!(t.take_title_changed());
}

#[test]
fn wheel_notches_follow_the_programs_mouse_mode_and_rows_keep_their_width() {
    let mut t = Terminal::new(20, 4, 1 << 20);
    assert!(t.wheel(true).is_empty(), "no mouse reports asked for");
    t.write(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    assert_eq!(String::from_utf8_lossy(&t.wheel(true)), "\x1b[<64;11;3M");
    assert_eq!(String::from_utf8_lossy(&t.wheel(false)), "\x1b[<65;11;3M");
    t.write(b"hello\r\nworld  \x1b[2;19H\xe2\x94\x82");
    let rows = t.screen_rows();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], format!("{:<20}", "hello"));
    assert!(rows[1].starts_with("world") && rows[1].chars().nth(18) == Some('│'), "{:?}", rows[1]);
    assert!(rows.iter().all(|r| r.chars().count() == 20));
}

#[test]
fn answers_queries_like_ghostty_with_viewer_facts() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.set_xtversion("WezTerm 20240203");
    t.set_display(Some([1, 2, 3]), Some([0xff, 0xff, 0xff]), None, Some((9, 18)));
    let ask = |t: &mut Terminal, q: &[u8]| {
        t.write(q);
        String::from_utf8_lossy(&t.take_responses()).into_owned()
    };
    assert_eq!(ask(&mut t, b"\x1b[c"), "\x1b[?62;22c");
    assert_eq!(ask(&mut t, b"\x1b[>c"), "\x1b[>1;10;0c");
    assert!(ask(&mut t, b"\x1b[>q").contains("WezTerm 20240203"));
    assert_eq!(ask(&mut t, b"\x1b[3;5H\x1b[6n"), "\x1b[3;5R");
    assert!(ask(&mut t, b"\x1b]11;?\x07").contains("rgb:ffff/ffff/ffff"));
    assert!(ask(&mut t, b"\x1b]10;?\x07").contains("rgb:0101/0202/0303"));
    assert_eq!(ask(&mut t, b"\x1b[?996n"), "\x1b[?997;2n", "light background");
    assert_eq!(ask(&mut t, b"\x1b[16t"), "\x1b[6;18;9t");
    assert_eq!(ask(&mut t, b"\x1b[18t"), "\x1b[8;24;80t");
    assert_eq!(ask(&mut t, b"\x1b[?2004$p"), "\x1b[?2004;2$y");
    assert_eq!(ask(&mut t, b"\x1b[?u"), "\x1b[?0u");
}

#[test]
fn grapheme_clustering_is_on_like_ghostty() {
    let mut t = Terminal::new(10, 2, 0);
    t.write(b"\x1b[?2027$p");
    assert_eq!(String::from_utf8_lossy(&t.take_responses()), "\x1b[?2027;1$y");
    t.write("\u{270d}\u{fe0f}!".as_bytes());
    assert_eq!(t.plain_text().trim_end(), "\u{270d}\u{fe0f}!");
    t.write(b"\x1bc");
    assert!(t.dec_mode(2027), "a reset keeps it on");
}

/// What the terminal answers to DECRQSS for the cursor shape.
fn decscusr(t: &mut Terminal) -> String {
    t.take_responses();
    t.write(b"\x1bP$q q\x1b\\");
    String::from_utf8_lossy(&t.take_responses()).into_owned()
}

#[test]
fn paint_replays_the_cursor_shape_the_program_chose() {
    // zsh-vi-mode in insert mode, nvim, Claude Code's vim mode: a bar.
    for (set, answer) in [("6", "6"), ("5", "5"), ("2", "2"), ("4", "4")] {
        let mut t = Terminal::new(80, 24, 0);
        t.write(format!("\x1b[{set} q").as_bytes());
        let mut viewer = Terminal::new(80, 24, 0);
        viewer.write(b"\x1b[3 q"); // whatever the viewer showed before
        viewer.write(&t.paint(0));
        assert_eq!(decscusr(&mut viewer), format!("\x1bP1$r{answer} q\x1b\\"), "after CSI {set} q");

        // A snapshot keeps it too.
        let restored = Terminal::restore(&t.snapshot().unwrap(), 0).unwrap();
        assert_eq!(restored.cursor_shape(), t.cursor_shape());
    }
}

#[test]
fn paint_leaves_the_viewers_default_cursor_when_the_program_chose_none() {
    let fresh = Terminal::new(80, 24, 0);
    let mut reset = Terminal::new(80, 24, 0);
    reset.write(b"\x1b[6 q\x1b[0 q");
    let mut soft = Terminal::new(80, 24, 0);
    soft.write(b"\x1b[6 q\x1b[!p");
    for t in [fresh, reset, soft] {
        assert_eq!(t.cursor_shape(), None);
        assert!(!String::from_utf8_lossy(&t.paint(0)).contains(" q"));
    }
    // The hollow marker never shows in what the core answers itself.
    assert_eq!(decscusr(&mut Terminal::new(80, 24, 0)), "\x1bP1$r2 q\x1b\\");
}

/// Each scrollback row of `t` as plain text, one by one: a blank row is an empty string,
/// even the last (formatting the rows together would drop trailing blank ones).
fn history_text(t: &Terminal) -> Vec<String> {
    let (cols, _) = t.size();
    (0..t.history_rows())
        .map(|y| {
            let sel = t.select(ffi::POINT_HISTORY, (0, y), (cols - 1, y)).unwrap();
            String::from_utf8_lossy(&t.format_with(ffi::FORMAT_PLAIN, Some(&sel), false, false)).trim_end().to_owned()
        })
        .collect()
}

/// A terminal with `lines` history rows ("history 0".."history N") under a full screen
/// ("screen 1".. and a prompt).
fn with_history(rows: u16, lines: usize) -> Terminal {
    let mut t = Terminal::new(20, rows, 1 << 20);
    for i in 0..lines {
        t.write(format!("history {i}\r\n").as_bytes());
    }
    for i in 1..rows {
        t.write(format!("screen {i}\r\n").as_bytes());
    }
    t.write(b"prompt> ");
    assert_eq!(t.history_rows() as usize, lines);
    t
}

#[test]
fn painted_history_meets_the_screen_with_no_blank_row_between() {
    // History shorter than the screen, as tall, taller, only part of it, and a one-row screen.
    for (rows, lines, painted) in [(4u16, 2usize, 100usize), (4, 4, 100), (4, 9, 100), (4, 9, 3), (1, 3, 100)] {
        let t = with_history(rows, lines);
        let mut viewer = Terminal::new(20, rows, 1 << 20);
        viewer.write(&t.paint(painted));
        let want: Vec<String> = (lines - painted.min(lines)..lines).map(|i| format!("history {i}")).collect();
        let got = history_text(&viewer);
        let blank = got.iter().filter(|r| r.is_empty()).count();
        let case = format!("{rows} rows, {lines} history rows, {painted} painted: {got:?}");
        // Every row counts: one blank row more than history is the seam above the screen.
        assert_eq!((viewer.history_rows() as usize, blank), (want.len(), 0), "{case}");
        assert_eq!(got, want, "{case}");
        assert_eq!(viewer.screen_rows(), t.screen_rows(), "{case}");
    }
}

#[test]
fn painted_history_fits_a_viewer_taller_or_shorter_than_the_terminal() {
    // A viewer never told this terminal's size (`hc attach` in a 6-row or 3-row
    // terminal, a 4-row PTY) renders the paint in its own rows.
    for lines in [3usize, 10] {
        let t = with_history(4, lines);
        let screen: Vec<String> = t.screen_rows().iter().map(|r| r.trim_end().to_owned()).collect();
        for viewer_rows in [3u16, 4, 6] {
            let mut viewer = Terminal::new(20, viewer_rows, 1 << 20);
            viewer.write(&t.paint_for(100, viewer_rows));
            let got = history_text(&viewer);
            let shown: Vec<String> = viewer.screen_rows().iter().map(|r| r.trim_end().to_owned()).collect();
            let case = format!("{viewer_rows}-row viewer, {lines} history rows: {got:?} | {shown:?}");
            // Every history row, no blank one. A screen taller than the viewer
            // overflows into its scrollback after them, as it always has.
            let overflow = 4usize.saturating_sub(viewer_rows as usize);
            let want: Vec<String> =
                (0..lines).map(|i| format!("history {i}")).chain(screen[..overflow].iter().cloned()).collect();
            assert_eq!(got, want, "{case}");
            let mut rest = screen[overflow..].to_vec();
            rest.resize(viewer_rows as usize, String::new());
            assert_eq!(shown, rest, "{case}");
        }
    }
}

/// Glyphs written, each with the hyperlink target active then.
type Glyph = (u8, Option<Vec<u8>>);
type Glyphs = Vec<Glyph>;

/// The OSC 8 hyperlinks in VT output, checked where they are placed, not
/// counted: every glyph with the target active when it is written, and no
/// link left open across a CR or LF, a cursor move or other CSI (SGR aside),
/// or the end of the paint. Returns the glyphs and each link's opening
/// parameters and target (`id=x;uri`). From the review of #104.
fn osc8_placement(bytes: &[u8]) -> (Glyphs, Vec<Vec<u8>>) {
    let (mut i, mut active, mut glyphs, mut links) = (0, None, Vec::new(), Vec::new());
    while i < bytes.len() {
        if bytes[i..].starts_with(b"\x1b]") {
            let start = i + 2;
            i = start;
            while i < bytes.len() && bytes[i] != 7 && !bytes[i..].starts_with(b"\x1b\\") {
                assert_ne!(bytes[i], 0x1b, "unescaped ESC inside an OSC");
                i += 1;
            }
            assert!(i < bytes.len(), "unterminated OSC");
            let body = &bytes[start..i];
            if let Some(rest) = body.strip_prefix(b"8;") {
                let split = rest.iter().position(|b| *b == b';').expect("OSC 8 parameters end with ;");
                if rest[split + 1..].is_empty() {
                    active = None;
                } else {
                    assert!(active.is_none(), "a link opened over an open one");
                    active = Some(rest[split + 1..].to_vec());
                    links.push(rest.to_vec());
                }
            }
            i += if bytes[i] == 7 { 1 } else { 2 };
        } else if bytes[i..].starts_with(b"\x1b[") {
            i += 2;
            while !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if bytes[i] != b'm' {
                assert!(active.is_none(), "a link open over CSI {:?}", bytes[i] as char);
            }
            i += 1;
        } else if bytes[i] == 0x1b {
            i += 2;
        } else if matches!(bytes[i], b'\r' | b'\n') {
            assert!(active.is_none(), "a link open across a CR or LF");
            i += 1;
        } else {
            glyphs.push((bytes[i], active.clone()));
            i += 1;
        }
    }
    assert!(active.is_none(), "the paint left a link open");
    (glyphs, links)
}

/// The glyphs of `text` in `glyphs`, each with the target it was written under.
fn linked<'a>(glyphs: &'a [Glyph], text: &str) -> Vec<&'a [Glyph]> {
    glyphs.windows(text.len()).filter(|w| w.iter().map(|g| g.0).eq(text.bytes())).collect()
}

/// Wolf's ask (11-18, phone links): a link printed before a viewer attached
/// opens there too. Claude Code prints a markdown link as only its text, the
/// URL in an OSC 8 hyperlink, and puts every row of a long URL it broke
/// itself in OSC 8 of the same id. A repaint dropped them all.
#[test]
fn wolf_ask_links_paint_keeps_osc8_hyperlinks_for_a_viewer_that_attaches_later() {
    const DOCS: &[u8] = b"https://x.dev/docs";
    const LONG: &[u8] = b"https://x.dev/0123456789abcdefghij";
    let mut t = Terminal::new(20, 6, 1 << 20);
    // As Claude Code writes them (BEL-terminated, with ids; indents by moving the cursor).
    t.write(b"Read \x1b]8;id=a;https://x.dev/docs\x07the docs\x1b]8;;\x07 now\r\n");
    for part in ["https://x.dev/0123", "456789abcdefghij"] {
        t.write(format!("\x1b[2C\x1b]8;id=b;https://x.dev/0123456789abcdefghij\x07{part}\x1b]8;;\x07\r\n").as_bytes());
    }
    t.write(b"plain https://not.linked");
    let paint = t.paint(0);
    let (glyphs, links) = osc8_placement(&paint);
    let all_under = |text: &str, target: Option<&[u8]>| {
        let found = linked(&glyphs, text);
        !found.is_empty() && found.iter().all(|w| w.iter().all(|g| g.1.as_deref() == target))
    };
    assert!(all_under("the docs", Some(DOCS)), "the markdown text keeps its target");
    assert!(all_under("Read ", None) && all_under(" now", None));
    assert!(all_under("https://x.dev/0123", Some(LONG)) && all_under("456789abcdefghij", Some(LONG)));
    assert!(all_under("plain https://not.linked", None), "text that never was a link");
    // Each row of the broken URL opens again with its id; the indents are not linked.
    assert_eq!(
        links,
        [
            b"id=a;https://x.dev/docs".to_vec(),
            [b"id=b;".as_slice(), LONG].concat(),
            [b"id=b;".as_slice(), LONG].concat()
        ]
    );
    assert!(String::from_utf8_lossy(&paint).contains("\r\n  \x1b]8;id=b;"));

    // A viewer painted from it carries the same links on.
    let mut viewer = Terminal::new(20, 6, 1 << 20);
    viewer.write(&paint);
    assert_eq!(osc8_placement(&viewer.paint(0)).1, links);

    // History lines keep theirs too.
    let mut tall = Terminal::new(20, 3, 1 << 20);
    tall.write(b"\x1b]8;;https://x.dev/docs\x07docs\x1b]8;;\x07\r\na\r\nb\r\nc\r\nd");
    let (glyphs, _) = osc8_placement(&tall.paint(10));
    assert!(linked(&glyphs, "docs").iter().all(|w| w.iter().all(|g| g.1.as_deref() == Some(DOCS))));
}

#[test]
fn wolf_ask_links_a_cursor_gap_after_a_link_is_not_linked() {
    // Review of #104: blanks were written as spaces under the open link.
    let mut t = Terminal::new(20, 3, 1 << 20);
    t.write(b"\x1b]8;;https://x.dev/docs\x1b\\LINK\x1b]8;;\x1b\\\x1b[3Cplain");
    let paint = t.paint(0);
    let (glyphs, _) = osc8_placement(&paint);
    assert!(String::from_utf8_lossy(&paint).contains("LINK\x1b]8;;\x1b\\   plain"));
    assert!(linked(&glyphs, "   plain")[0].iter().all(|g| g.1.is_none()));
}

#[test]
fn wolf_ask_links_a_soft_wrapped_link_in_history_ends_with_its_row() {
    // Review of #104: joined history rows did not close the link at the
    // physical row, so the next row's indent was linked.
    let mut t = Terminal::new(10, 3, 1 << 20);
    // 'x' wraps; erase it and make an unlinked indent on the continuation row.
    t.write(b"\x1b]8;;https://x.dev/docs\x1b\\abcdefghij\x1b]8;;\x1b\\x\r\x1b[K\x1b[2Ctail\r\none\r\ntwo\r\nthree");
    let paint = t.paint(100);
    osc8_placement(&paint);
    assert!(String::from_utf8_lossy(&paint).contains("abcdefghij\x1b]8;;\x1b\\  tail"));
    // A link without an id, soft-wrapped, reopens as the same link.
    let mut t = Terminal::new(10, 3, 1 << 20);
    t.write(b"\x1b]8;;https://x.dev/docs\x1b\\abcdefghijklm\x1b]8;;\x1b\\\r\none\r\ntwo\r\nthree");
    let (_, links) = osc8_placement(&t.paint(100));
    assert_eq!(links.len(), 2, "{links:?}");
    assert_eq!(links[0], links[1], "one id for both rows");
    assert!(links[0].starts_with(b"id=herdr.i."));
}

#[test]
fn wolf_ask_links_explicit_ids_are_kept_across_rows() {
    let mut t = Terminal::new(20, 4, 1 << 20);
    for s in ["first", "second"] {
        t.write(format!("\x1b]8;id=claude;https://x.dev/docs\x07{s}\x1b]8;;\x07\r\n").as_bytes());
    }
    // An id that is not plain ASCII is dropped, never written raw.
    t.write("\x1b]8;id=é;https://x.dev/e\x07third\x1b]8;;\x07".as_bytes());
    let (_, links) = osc8_placement(&t.paint(0));
    assert_eq!(
        links,
        [
            b"id=claude;https://x.dev/docs".to_vec(),
            b"id=claude;https://x.dev/docs".to_vec(),
            b";https://x.dev/e".to_vec()
        ]
    );
}

#[test]
fn wolf_ask_links_targets_are_portable_ascii_without_double_encoding() {
    let mut t = Terminal::new(20, 3, 1 << 20);
    t.write("\x1b]8;;https://x.dev/Ü;a:b%20é\x1b\\LINK\x1b]8;;\x1b\\plain".as_bytes());
    let (_, links) = osc8_placement(&t.paint(0));
    // Implicit (no id given): the stable herdr.i.N id, then the encoded target.
    assert_eq!(links.len(), 1);
    assert!(links[0].starts_with(b"id=herdr.i."), "{links:?}");
    assert!(links[0].ends_with(b";https://x.dev/%C3%9C;a:b%20%C3%A9"), "{links:?}");
    assert!(links[0].iter().all(|b| (0x21..=0x7e).contains(b)));
}

#[test]
fn wolf_ask_links_links_close_at_every_boundary_of_every_kind_of_paint() {
    // From the review of #104: main and alternate screen, BEL and ST input,
    // history pages, a pending wrap at the edge, and a plain paint after.
    let uri = "https://x.dev/a;b:c%20d?q=é中&z=%1b%07#frag";
    let portable = b"https://x.dev/a;b:c%20d?q=%C3%A9%E4%B8%AD&z=%1b%07#frag";
    for alt in [false, true] {
        for end in ["\x07", "\x1b\\"] {
            let mut t = Terminal::new(20, 5, 8 << 20);
            if alt {
                t.write(b"\x1b[?1049h");
            }
            for n in 0..100 {
                t.write(format!("\x1b]8;id=same;{uri}{end}LINK{n}中\x1b]8;;{end}\r\n").as_bytes());
            }
            t.write(b"\x1b[3Cplain\r\n\r\nprompt> ");
            let paint = t.paint(1000);
            let (glyphs, links) = osc8_placement(&paint);
            assert!(!links.is_empty() && links.iter().all(|l| l.ends_with(portable)));
            assert!(linked(&glyphs, "prompt>")[0].iter().all(|g| g.1.is_none()));
            let mut viewer = Terminal::new(20, 5, 8 << 20);
            viewer.write(&paint);
            assert_eq!(viewer.alt_screen(), alt);
            assert_eq!(viewer.screen_rows(), t.screen_rows());
            osc8_placement(&viewer.paint(1000));
            viewer.write(b"NEXT");
            let (glyphs, _) = osc8_placement(&viewer.paint(0));
            assert!(linked(&glyphs, "NEXT")[0].iter().all(|g| g.1.is_none()));
        }
    }
    for history in [0, 10] {
        let mut t = Terminal::new(10, 3, 1 << 20);
        t.write(b"\x1b]8;;https://x.dev/docs\x1b\\abcdefghij\x1b]8;;\x1b\\\r\n\r\nplain\r\n");
        // A pending wrap at the right edge after the program closed its link.
        t.write(b"\x1b]8;;https://x.dev/docs\x1b\\0123456789\x1b]8;;\x1b\\");
        let paint = t.paint(history);
        osc8_placement(&paint);
        let mut viewer = Terminal::new(10, 3, 1 << 20);
        viewer.write(&paint);
        assert_eq!(viewer.screen_rows(), t.screen_rows());
        osc8_placement(&viewer.paint(history));
        // The next paint, all plain, leaves no link behind.
        viewer.write(&Terminal::new(10, 3, 1 << 20).paint(0));
        viewer.write(b"prompt> ");
        assert!(osc8_placement(&viewer.paint(0)).1.is_empty());
    }
}

/// Every OSC 8 opening in `bytes`, as `params;target`, in order.
fn osc8_opens(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut opens = Vec::new();
    let mut rest = bytes;
    while let Some(at) = rest.windows(4).position(|w| w == b"\x1b]8;") {
        let body = &rest[at + 4..];
        let end = body.windows(2).position(|w| w == b"\x1b\\").expect("an ST ends the OSC");
        let body = &body[..end];
        if !body.ends_with(b";") {
            opens.push(body.to_vec());
        }
        rest = &rest[at + 4 + end..];
    }
    opens
}

#[test]
fn wolf_ask_links_a_link_too_long_once_encoded_is_left_out_whole() {
    // Review of #104, round 2: the parser's 2048-byte capture holds the
    // parameters, a ";", the target and a NUL, encoded; the phone drops a
    // target past 4096. Over it: no id, then no link, never a cut target.
    let paint_of = |input: String| {
        let mut t = Terminal::new(40, 3, 1 << 20);
        t.write(input.as_bytes());
        t.paint(0)
    };
    // 400 "é": 814 bytes kept by the source, 2414 encoded.
    let wide = format!("https://x.dev/{}", "é".repeat(400));
    let paint = paint_of(format!("\x1b]8;id=a;{wide}\x07LINK\x1b]8;;\x07 plain"));
    let (glyphs, links) = osc8_placement(&paint);
    assert!(links.is_empty(), "{links:?}");
    assert!(linked(&glyphs, "LINK plain")[0].iter().all(|g| g.1.is_none()));
    // 800 "é" with Claude's id: 1614 bytes, 4814 encoded (tappable live, dead painted before).
    let wider = format!("https://x.dev/{}", "é".repeat(800));
    assert!(osc8_opens(&paint_of(format!("\x1b]8;id=sg4g0w;{wider}\x07LINK\x1b]8;;\x07"))).is_empty());
    // 2037 ASCII bytes, no id: kept whole, without the implicit id that would not fit.
    let long = format!("https://x.dev/{}", "x".repeat(2037 - 14));
    let opens = osc8_opens(&paint_of(format!("\x1b]8;;{long}\x07LINK\x1b]8;;\x07")));
    assert_eq!(opens, [format!(";{long}").into_bytes()]);
    // Short enough for its id too: both kept.
    let fits = format!("https://x.dev/{}", "x".repeat(2000));
    let opens = osc8_opens(&paint_of(format!("\x1b]8;id=a;{fits}\x07LINK\x1b]8;;\x07")));
    assert_eq!(opens, [format!("id=a;{fits}").into_bytes()]);
}

#[test]
fn wolf_ask_links_a_repaint_drops_a_link_the_viewer_missed_the_close_of() {
    // Review of #104, round 2 (inherited): a viewer painted while the
    // program was inside a link, which then closed it while the viewer fell
    // behind, kept the old link for the repaint's text and the live text
    // after it. Both a Ghostty viewer and the phone tapped "plainNEXT" open.
    let mut source = Terminal::new(20, 3, 1 << 20);
    source.write(b"\x1b]8;id=sg4g0w;https://x.dev/docs\x1b\\LINK");
    let mut viewer = Terminal::new(20, 3, 1 << 20);
    viewer.write(&source.paint(0));
    source.write(b"\x1b]8;;\x1b\\\x1b[2J\x1b[Hplain");
    viewer.write(&source.paint(0));
    viewer.write(b"NEXT");
    let (glyphs, links) = osc8_placement(&viewer.paint(0));
    assert!(links.is_empty(), "{links:?}");
    assert!(linked(&glyphs, "plainNEXT")[0].iter().all(|g| g.1.is_none()));
    // The program's own active link still reaches live text after a paint.
    let mut source = Terminal::new(20, 3, 1 << 20);
    source.write(b"\x1b]8;id=sg4g0w;https://x.dev/docs\x1b\\LINK");
    let mut viewer = Terminal::new(20, 3, 1 << 20);
    viewer.write(&source.paint(0));
    viewer.write(b"MORE");
    let opens = osc8_opens(&viewer.paint(0));
    assert!(!opens.is_empty() && opens.iter().all(|o| o == b"id=sg4g0w;https://x.dev/docs"), "{opens:?}");
}

#[test]
fn wolf_ask_links_cells_and_their_active_continuation_share_one_id_none_a_programs() {
    // Review of #104, round 2: an implicit link still open at the paint gets
    // the same id on its cells and on the cursor's continuation.
    let mut t = Terminal::new(20, 3, 1 << 20);
    t.write(b"\x1b]8;;https://x.dev/c\x1b\\abc");
    let opens = osc8_opens(&t.paint(0));
    assert!(opens.len() >= 2, "{opens:?}");
    assert!(opens[0].starts_with(b"id=herdr.i.") && opens.iter().all(|o| *o == opens[0]), "{opens:?}");
    // A program's id in the implicit ids' namespace is dropped, not shared.
    let mut t = Terminal::new(20, 3, 1 << 20);
    t.write(b"\x1b]8;;https://x.dev/c\x1b\\abc\x1b]8;;\x1b\\\r\n");
    t.write(b"\x1b]8;id=herdr.i.0;https://x.dev/c\x1b\\def\x1b]8;;\x1b\\");
    let (_, links) = osc8_placement(&t.paint(0));
    assert_eq!(links.len(), 2, "{links:?}");
    assert!(links[0].starts_with(b"id=herdr.i."));
    assert_eq!(links[1], b";https://x.dev/c".to_vec());
}

#[test]
fn wolf_ask_links_a_repaint_closes_a_link_the_main_screen_kept_under_the_alternate_one() {
    // Review of #104, round 3: the first close lands on the screen the viewer
    // is on; back on the main screen, its restored cursor carried the old
    // link, and the phone tapped "plain" and the live "NEXT" open.
    let reset = PAINT_RESET;
    let main_back = reset.windows(6).position(|w| w == b"\x1b[?47l").expect("the reset leaves the alternate screen");
    let later = &reset[main_back..];
    let close = later.windows(7).position(|w| w == b"\x1b]8;;\x1b\\").expect("and closes links after");
    assert!(close < later.windows(7).position(|w| w == b"\x1b[H\x1b[2J").expect("before its clear"));
    // The case itself, on a viewer.
    let mut source = Terminal::new(20, 3, 1 << 20);
    source.write(b"\x1b]8;id=sg4g0w;https://x.dev/docs\x1b\\LINK");
    let mut viewer = Terminal::new(20, 3, 1 << 20);
    viewer.write(&source.paint(0));
    // The viewer sees the program go to the alternate screen, then misses the rest.
    viewer.write(b"\x1b[?1049halt");
    source.write(b"\x1b[?1049halt\x1b[?1049l\x1b]8;;\x1b\\\x1b[2J\x1b[Hplain");
    viewer.write(&source.paint(0));
    viewer.write(b"NEXT");
    let (glyphs, links) = osc8_placement(&viewer.paint(0));
    assert!(links.is_empty(), "{links:?}");
    assert!(linked(&glyphs, "plainNEXT")[0].iter().all(|g| g.1.is_none()));
}

/// `bytes` without its OSC 8 sequences.
fn without_links(bytes: &[u8]) -> Vec<u8> {
    let (mut out, mut at) = (Vec::new(), 0);
    for (span, _) in osc8_spans(bytes, 0) {
        out.extend_from_slice(&bytes[at..span.start]);
        at = span.end;
    }
    out.extend_from_slice(&bytes[at..]);
    out
}

/// The bytes of a paint's OSC 8 sequences after its reset.
fn link_bytes(paint: &[u8]) -> usize {
    osc8_spans(paint, PAINT_RESET.len()).iter().map(|(span, _)| span.len()).sum()
}

/// As link-dense as the history that made repaints 60 times bigger
/// (handover-blank-1118): four links a row, with ids, targets of 600 bytes.
fn link_dense(rows: usize) -> Terminal {
    let mut t = Terminal::new(120, 10, 64 << 20);
    let pad = "p".repeat(600);
    for r in 0..rows {
        let mut line = String::new();
        for k in 0..4 {
            line += &format!("\x1b]8;id=r{r}k{k};https://x.dev/{r}/{k}/{pad}\x1b\\L{r}.{k}\x1b]8;;\x1b\\ ");
        }
        t.write(format!("{line}\r\n").as_bytes());
    }
    t
}

#[test]
fn wolf_ask_links_a_link_dense_paint_keeps_its_newest_links_whole_within_the_budget() {
    let t = link_dense(2500);
    let full = t.paint_unbounded(2000, 10);
    let paint = t.paint(2000);
    // At most as many link bytes as other bytes, here under the cap.
    let other = full.len() - link_bytes(&full);
    assert!(other < PAINT_LINKS_MAX && link_bytes(&full) > 50 * other);
    assert!(link_bytes(&paint) <= paint_links_budget(other) && paint.len() <= 2 * other);
    // Not a byte of the text, modes or cursor is lost or cut.
    assert_eq!(without_links(&paint), without_links(&full));
    // Placed right, every OSC whole, and no link open at the end.
    let (glyphs, links) = osc8_placement(&paint);
    // The oldest text is plain; from the first link kept on, every link keeps its id and target.
    let pad = "p".repeat(600);
    let (mut plain, mut kept) = (0, 0);
    for r in 0..2500 {
        for k in 0..4 {
            let text = format!("L{r}.{k}");
            let Some(found) = linked(&glyphs, &text).first().copied() else { continue };
            let target = format!("https://x.dev/{r}/{k}/{pad}").into_bytes();
            if found.iter().all(|g| g.1.as_ref() == Some(&target)) {
                assert_eq!(links[kept], [format!("id=r{r}k{k};").into_bytes(), target].concat());
                kept += 1;
            } else {
                assert!(found.iter().all(|g| g.1.is_none()), "{text} is part linked");
                assert_eq!(kept, 0, "{text} is plain after a link");
                plain += 1;
            }
        }
    }
    assert!(plain > 0 && kept > 0 && kept == links.len(), "{plain} plain, {kept} of {}", links.len());
    // The rows on the screen keep theirs; the oldest painted history's are plain.
    let (screen, oldest) = (t.screen_rows(), "L500.0");
    let on_screen: Vec<String> = (0..2500)
        .flat_map(|r| (0..4).map(move |k| format!("L{r}.{k}")))
        .filter(|l| screen.iter().any(|row| row.contains(&format!("{l} "))))
        .collect();
    assert!(!on_screen.is_empty());
    for l in &on_screen {
        assert!(linked(&glyphs, l)[0].iter().all(|g| g.1.is_some()), "{l} on the screen is plain");
    }
    assert!(linked(&glyphs, oldest)[0].iter().all(|g| g.1.is_none()));
    // A viewer shows the same screen, at ground: live text after the paint is plain text.
    let mut viewer = Terminal::new(120, 10, 64 << 20);
    viewer.write(&paint);
    assert_eq!(viewer.screen_rows(), t.screen_rows());
    viewer.write(b"NEXT");
    let (glyphs, _) = osc8_placement(&viewer.paint(0));
    assert!(linked(&glyphs, "NEXT")[0].iter().all(|g| g.1.is_none()));
}

/// The rows of a paint whose links it keeps (`L<row>`), from the review of #112.
fn rows_keeping_links(paint: &[u8]) -> std::collections::BTreeSet<String> {
    let (glyphs, _) = osc8_placement(paint);
    let text: Vec<u8> = glyphs.iter().map(|g| g.0).collect();
    let mut rows = std::collections::BTreeSet::new();
    for (i, g) in glyphs.iter().enumerate() {
        if g.1.is_some() && text[i] == b'L' {
            let label: String = text[i..].iter().take_while(|b| **b != b'.').map(|b| *b as char).collect();
            rows.insert(label);
        }
    }
    rows
}

#[test]
fn wolf_ask_links_r112_over_the_budget_the_screen_keeps_its_links() {
    // Review of #112: the budget went to the oldest history first, and no
    // row on the screen kept its links.
    let t = link_dense(2500);
    let rows = rows_keeping_links(&t.paint(2000));
    assert!(!rows.is_empty());
    for row in t.screen_rows().iter().filter(|row| row.starts_with('L')) {
        let label = row.split('.').next().unwrap();
        assert!(rows.contains(label), "screen row {label} lost its links; kept {rows:?}");
    }
    // The newest history rows keep theirs; the oldest painted ones do not.
    assert!(rows.contains("L2490") && !rows.contains("L500"), "{rows:?}");
}

#[test]
fn wolf_ask_links_over_the_budget_the_cursors_active_link_is_kept() {
    // The cursor's active link is the newest: kept whenever any link is, so
    // live text after the paint still continues it (review of #104, round 4).
    let mut t = link_dense(2500);
    t.write(b"\x1b]8;id=live;https://x.dev/live\x1b\\LIVE");
    let paint = t.paint(2000);
    let full = t.paint_unbounded(2000, 10);
    assert!(link_bytes(&paint) <= paint_links_budget(full.len() - link_bytes(&full)));
    assert_eq!(osc8_opens(&paint).last().unwrap(), b"id=live;https://x.dev/live");
    let mut viewer = Terminal::new(120, 10, 64 << 20);
    viewer.write(&paint);
    viewer.write(b"MORE");
    let opens = osc8_opens(&viewer.paint(0));
    assert_eq!(opens.last().unwrap(), b"id=live;https://x.dev/live", "MORE continues the link");
    // Without an active link, a paint over the budget ends with none open.
    let t = link_dense(2500);
    osc8_placement(&t.paint(2000));
    // Within the budget, the paint is #104's.
    let mut t = link_dense(3);
    t.write(b"\x1b]8;id=live;https://x.dev/live\x1b\\LIVE");
    let paint = t.paint(2000);
    assert_eq!(paint, t.paint_unbounded(2000, 10));
    assert_eq!(osc8_opens(&paint).last().unwrap(), b"id=live;https://x.dev/live");
}

#[test]
fn wolf_ask_links_the_link_budget_holds_at_its_boundary() {
    assert_eq!(paint_links_budget(0), PAINT_LINKS_MIN);
    assert_eq!(paint_links_budget(100_000), 100_000);
    assert_eq!(paint_links_budget(1 << 30), PAINT_LINKS_MAX);
    // 32 bytes of link each (a 25-byte open, a 7-byte close) around one glyph.
    let link = |n: usize| format!("\x1b]8;;https://x.dev/{n:04}\x1b\\x\x1b]8;;\x1b\\");
    // Links `from..to`, the ones before `from` written as their plain glyph.
    let paint_of = |text: usize, from: usize, to: usize, tail: &str| {
        let links: String = (0..to).map(|n| if n < from { "x".to_string() } else { link(n) }).collect();
        [PAINT_RESET, "t".repeat(text).as_bytes(), links.as_bytes(), tail.as_bytes()].concat()
    };
    // A 32-byte open and its close: 39 bytes.
    let open = "\x1b]8;id=live;https://x.dev/live\x1b\\LIVE";
    for (text, budget) in [(0, PAINT_LINKS_MIN), (200 << 10, PAINT_LINKS_MAX)] {
        let fit = budget / 32;
        assert_eq!(link_bytes(&paint_of(text, 0, fit, "")), budget);
        // Exactly the budget: kept as it is.
        assert_eq!(bound_paint_links(paint_of(text, 0, fit, "")), paint_of(text, 0, fit, ""));
        // One more link: the oldest alone is left out, its glyph kept.
        assert_eq!(bound_paint_links(paint_of(text, 0, fit + 1, "")), paint_of(text, 1, fit + 1, ""));
        // A link still open at the end is kept, and the oldest that no longer fit go.
        assert_eq!(bound_paint_links(paint_of(text, 0, fit, open)), paint_of(text, 2, fit, open));
        assert_eq!(bound_paint_links(paint_of(text, 0, 10, open)), paint_of(text, 0, 10, open));
        // Never more than the budget, never part of a sequence.
        for links in [fit - 1, fit + 1, 2 * fit] {
            let bounded = bound_paint_links(paint_of(text, 0, links, open));
            assert!(link_bytes(&bounded) <= budget);
            assert_eq!(without_links(&bounded), without_links(&paint_of(text, 0, links, open)));
            assert!(osc8_opens(&bounded).last().is_some_and(|o| o == b"id=live;https://x.dev/live"));
        }
    }
    // An open is at most the parser's 2 KiB capture, under the smallest
    // budget: the newest link, however long, is always kept.
    let target = format!("https://x.dev/{}", "y".repeat(2000));
    let huge = format!("\x1b]8;;{target}\x1b\\LIVE");
    let paint = [PAINT_RESET, (0..600).map(link).collect::<String>().as_bytes(), huge.as_bytes()].concat();
    let bounded = bound_paint_links(paint.clone());
    assert!(link_bytes(&bounded) <= PAINT_LINKS_MIN);
    assert_eq!(osc8_opens(&bounded).last().unwrap(), format!(";{target}").as_bytes());
    assert_eq!(without_links(&bounded), without_links(&paint));
}
