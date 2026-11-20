//! UI-independent embedded terminal crate built on `libghostty-vt` and `portable-pty`.
//!
//! All modules in this library crate are UI-independent; the GPUI manual-check
//! window lives in `examples/demo.rs`.

pub mod keys;
pub mod mouse;
pub mod selection;
pub mod snapshot;
mod sys;
pub mod terminal;

pub use keys::{TermKey, TermKeyAction, TermKeyEvent, TermModifiers};
pub use mouse::{MouseOutcome, MousePosition, TermMouseButton, TermMouseEvent, TermMouseKind};
pub use selection::{BufferPoint, TerminalSelection};
pub use snapshot::{
    CellAttributes, CellSnapshot, CellWidth, CursorShape, CursorSnapshot, ExitStatus, Rgb,
    RowSnapshot, ScrollbarSnapshot, TerminalSnapshot, TerminalTheme, UnderlineStyle,
};
pub use terminal::{
    EventCallback, ScrollAction, TermError, TerminalCore, TerminalEvent, TerminalOptions,
    TerminalSession, abi_type_manifest_json, library_version,
};

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn test_core(cols: u16, rows: u16) -> TerminalCore {
        TerminalCore::new(&TerminalOptions {
            cols,
            rows,
            ..TerminalOptions::default()
        })
        .expect("create TerminalCore")
    }

    #[cfg(not(windows))]
    fn sh_options(cols: u16, rows: u16, cmd: &str) -> TerminalOptions {
        TerminalOptions {
            cols,
            rows,
            shell: Some("/bin/sh".to_string()),
            shell_args: vec!["-c".to_string(), cmd.to_string()],
            ..TerminalOptions::default()
        }
    }

    #[test]
    fn library_version_and_abi_manifest() {
        let version = library_version();
        assert_eq!(version, "1.0.1");

        let manifest = abi_type_manifest_json();
        assert!(manifest.contains("\"GhosttyRenderStateCursor\""));
        assert!(manifest.contains("\"GhosttyStyle\""));
    }

    #[test]
    fn vt_colors_and_attributes() {
        let mut core = test_core(40, 10);
        // Truecolor fg (12, 34, 56), truecolor bg (78, 90, 12), bold + italic + underline.
        core.feed_bytes(b"\x1b[1;3;4;38;2;12;34;56;48;2;78;90;12mA\x1b[0m\x1b[7mB\x1b[0m");
        let snap = core.snapshot(false, None).expect("snapshot");

        let cell_a = &snap.grid[0].cells[0];
        assert_eq!(cell_a.text, "A");
        assert_eq!(cell_a.fg, Rgb::new(12, 34, 56));
        assert_eq!(cell_a.bg, Rgb::new(78, 90, 12));
        assert!(cell_a.has_explicit_bg);
        assert!(cell_a.attrs.bold);
        assert!(cell_a.attrs.italic);
        assert_eq!(cell_a.attrs.underline, UnderlineStyle::Single);
        assert!(!cell_a.attrs.inverse);
        assert_eq!(cell_a.effective_fg(), Rgb::new(12, 34, 56));
        assert_eq!(cell_a.effective_bg(), Rgb::new(78, 90, 12));

        let cell_b = &snap.grid[0].cells[1];
        assert_eq!(cell_b.text, "B");
        assert!(cell_b.attrs.inverse);
        assert_eq!(cell_b.effective_fg(), snap.default_bg);
        assert_eq!(cell_b.effective_bg(), snap.default_fg);
    }

    #[test]
    fn vt_cursor_movement_and_shape() {
        let mut core = test_core(20, 8);
        // Move to row 3, col 5 (1-based) and set bar cursor (DECSCUSR 6).
        core.feed_bytes(b"\x1b[3;5H\x1b[6 q");
        let snap = core.snapshot(false, None).expect("snapshot");
        assert!(snap.cursor.in_viewport);
        assert!(snap.cursor.visible);
        assert_eq!(snap.cursor.row, 2);
        assert_eq!(snap.cursor.col, 4);
        assert_eq!(snap.cursor.shape, CursorShape::Bar);

        // Hide cursor (DECTCEM reset) and switch to underline cursor (DECSCUSR 4).
        core.feed_bytes(b"\x1b[?25l\x1b[4 q");
        let snap = core.snapshot(false, None).expect("snapshot");
        assert!(!snap.cursor.visible);
        assert_eq!(snap.cursor.shape, CursorShape::Underline);
    }

    #[test]
    fn vt_line_wrap() {
        let mut core = test_core(5, 4);
        core.feed_bytes(b"abcdefgh");
        let snap = core.snapshot(false, None).expect("snapshot");

        assert_eq!(snap.row_text(0), "abcde");
        assert!(snap.grid[0].wrap);
        assert!(!snap.grid[0].wrap_continuation);

        assert_eq!(snap.row_text(1), "fgh");
        assert!(!snap.grid[1].wrap);
        assert!(snap.grid[1].wrap_continuation);
    }

    #[test]
    fn vt_alternate_screen() {
        let mut core = test_core(20, 5);
        core.feed_bytes(b"primary-line");
        assert!(!core.snapshot(false, None).expect("snap").alternate_screen);

        // Enter alt screen, write distinct content.
        core.feed_bytes(b"\x1b[?1049h\x1b[2J\x1b[Halt-screen");
        let alt_snap = core.snapshot(false, None).expect("alt snap");
        assert!(alt_snap.alternate_screen);
        assert_eq!(alt_snap.row_text(0), "alt-screen");

        // Exit alt screen; primary buffer returns intact.
        core.feed_bytes(b"\x1b[?1049l");
        let restored = core.snapshot(false, None).expect("restored snap");
        assert!(!restored.alternate_screen);
        assert_eq!(restored.row_text(0), "primary-line");
    }

    #[test]
    fn vt_utf8_and_cjk_wide_chars() {
        let mut core = test_core(16, 4);
        // Combining mark + CJK wide characters ("e\u{0301} 日本語").
        core.feed_bytes("e\u{0301} 日本語".as_bytes());
        let snap = core.snapshot(false, None).expect("snapshot");

        assert_eq!(snap.grid[0].cells[0].text, "e\u{0301}");
        assert_eq!(snap.grid[0].cells[0].width, CellWidth::Narrow);

        // '日' at col 2 (wide) followed by spacer tail at col 3.
        assert_eq!(snap.grid[0].cells[2].text, "日");
        assert_eq!(snap.grid[0].cells[2].width, CellWidth::Wide);
        assert_eq!(snap.grid[0].cells[3].width, CellWidth::SpacerTail);

        assert_eq!(snap.grid[0].cells[4].text, "本");
        assert_eq!(snap.grid[0].cells[4].width, CellWidth::Wide);
        assert_eq!(snap.grid[0].cells[6].text, "語");
        assert_eq!(snap.grid[0].cells[6].width, CellWidth::Wide);
        assert_eq!(snap.row_text(0), "e\u{0301} 日本語");
        assert_eq!(snap.cursor.col, 8);
    }

    #[test]
    fn vt_scrollback_and_viewport_scroll() {
        let mut core = test_core(20, 4);
        for i in 0..12 {
            core.feed_bytes(format!("line-{i:02}\r\n").as_bytes());
        }

        let live_snap = core.snapshot(false, None).expect("live snap");
        assert!(live_snap.scrollback_rows >= 8);
        assert!(live_snap.viewport_active);
        assert!(live_snap.scrollbar.is_at_bottom());
        assert!(live_snap.scrollbar.is_scrollable());
        assert_eq!(live_snap.row_text(0), "line-09");
        assert_eq!(live_snap.row_text(2), "line-11");

        let history = core.read_scrollback_rows(0, 3).expect("history");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].text(), "line-00");
        assert_eq!(history[1].text(), "line-01");
        assert_eq!(history[2].text(), "line-02");

        core.scroll_viewport(ScrollAction::Top).expect("scroll top");
        let top_snap = core.snapshot(false, None).expect("top snap");
        assert!(!top_snap.viewport_active);
        assert!(!top_snap.scrollbar.is_at_bottom());
        assert_eq!(top_snap.row_text(0), "line-00");

        core.scroll_viewport(ScrollAction::PageDown)
            .expect("page down");
        let paged_snap = core.snapshot(false, None).expect("paged snap");
        assert_eq!(paged_snap.row_text(0), "line-04");

        core.scroll_viewport(ScrollAction::PageUp).expect("page up");
        let back_top = core.snapshot(false, None).expect("back top");
        assert_eq!(back_top.row_text(0), "line-00");

        core.scroll_viewport(ScrollAction::Bottom)
            .expect("scroll bottom");
        let bottom_snap = core.snapshot(false, None).expect("bottom snap");
        assert!(bottom_snap.viewport_active);
        assert!(bottom_snap.scrollbar.is_at_bottom());
        assert_eq!(bottom_snap.row_text(0), "line-09");
    }

    #[test]
    fn vt_key_encoder_sequences() {
        let mut core = test_core(40, 10);

        let up = TermKeyEvent {
            action: TermKeyAction::Press,
            key: TermKey::ArrowUp,
            ..TermKeyEvent::default()
        };
        assert_eq!(core.encode_key(&up).expect("encode up"), b"\x1b[A");

        // Enable DECCKM (application cursor keys).
        core.feed_bytes(b"\x1b[?1h");
        assert_eq!(core.encode_key(&up).expect("encode app up"), b"\x1bOA");

        let ctrl_c = TermKeyEvent {
            action: TermKeyAction::Press,
            key: TermKey::C,
            modifiers: TermModifiers {
                ctrl: true,
                ..TermModifiers::default()
            },
            text: Some("c".to_string()),
            ..TermKeyEvent::default()
        };
        assert_eq!(core.encode_key(&ctrl_c).expect("encode ctrl+c"), b"\x03");

        let tab = TermKeyEvent {
            action: TermKeyAction::Press,
            key: TermKey::Tab,
            text: Some("\t".to_string()),
            ..TermKeyEvent::default()
        };
        assert_eq!(core.encode_key(&tab).expect("encode tab"), b"\t");

        let shift_tab = TermKeyEvent {
            action: TermKeyAction::Press,
            key: TermKey::Tab,
            modifiers: TermModifiers {
                shift: true,
                ..TermModifiers::default()
            },
            ..TermKeyEvent::default()
        };
        assert_eq!(
            core.encode_key(&shift_tab).expect("encode shift+tab"),
            b"\x1b[Z"
        );
    }

    #[test]
    fn selection_text_wrap_joining_and_trim() {
        let mut core = test_core(6, 5);
        core.feed_bytes(b"abcdefghij\r\nsecond   \r\n\r\n");

        // Line 0 wraps onto row 1 ("abcdef" + "ghij"), so selecting across the
        // wrapped boundary must join without inserting a newline and trim trailing spaces.
        let sel = TerminalSelection::new(BufferPoint::new(0, 0), BufferPoint::new(2, 5));
        let text = core.selected_text(&sel).expect("selected_text");
        assert_eq!(text, "abcdefghij\nsecond");

        // Line selection helper captures the entire wrapped logical line cleanly.
        let line_sel = core
            .select_line(BufferPoint::new(1, 2))
            .expect("select_line")
            .expect("line selected");
        assert_eq!(line_sel.start, BufferPoint::new(0, 0));
        assert_eq!(
            core.selected_text(&line_sel).expect("line text"),
            "abcdefghij"
        );

        // Word selection helper selects the word under the coordinate.
        let word_sel = core
            .select_word(BufferPoint::new(2, 2))
            .expect("select_word")
            .expect("word selected");
        assert_eq!(core.selected_text(&word_sel).expect("word text"), "second");
    }

    #[test]
    fn selection_wide_chars_and_scrollback_range() {
        let mut core = test_core(12, 4);
        core.feed_bytes("A日本語B\r\n".as_bytes());
        for i in 1..8 {
            core.feed_bytes(format!("row-{i}\r\n").as_bytes());
        }

        // Row 0 is now deep in scrollback history; select "日本語" including spacer tail.
        let snap = core.snapshot(false, None).expect("snap");
        assert!(snap.scrollback_rows >= 4);

        let wide_sel = TerminalSelection::new(BufferPoint::new(0, 1), BufferPoint::new(0, 6));
        assert_eq!(core.selected_text(&wide_sel).expect("wide text"), "日本語");

        // Reversed drag direction (end before start) spanning scrollback into active area.
        let span_sel = TerminalSelection::new(BufferPoint::new(2, 4), BufferPoint::new(0, 0));
        let span_text = core.selected_text(&span_sel).expect("scrollback span");
        assert_eq!(span_text, "A日本語B\nrow-1\nrow-2");

        // Verify pure-Rust UI row range helper across rows.
        assert_eq!(span_sel.row_col_range(0, 12), Some(0..=11));
        assert_eq!(span_sel.row_col_range(1, 12), Some(0..=11));
        assert_eq!(span_sel.row_col_range(2, 12), Some(0..=4));
        assert_eq!(span_sel.row_col_range(3, 12), None);
    }

    #[test]
    fn bracketed_paste_and_focus_encoding() {
        let mut core = test_core(40, 10);

        // Mode 2004 off: newlines become CR and ESC bytes are scrubbed.
        assert!(!core.snapshot(false, None).expect("snap").bracketed_paste);
        let plain = core.encode_paste("a\nb\x1b").expect("plain paste");
        assert_eq!(plain, b"a\rb ");

        // Mode 2004 on: wrapped in ESC[200~ ... ESC[201~, preserving newlines inside.
        core.feed_bytes(b"\x1b[?2004h");
        assert!(core.snapshot(false, None).expect("snap").bracketed_paste);
        let bracketed = core.encode_paste("hello\nworld").expect("bracketed paste");
        assert_eq!(bracketed, b"\x1b[200~hello\nworld\x1b[201~");

        // Mode 1004 off: focus events emit nothing.
        assert!(!core.snapshot(false, None).expect("snap").focus_reporting);
        assert_eq!(core.encode_focus(true).expect("focus off"), None);

        // Mode 1004 on: CSI I / CSI O.
        core.feed_bytes(b"\x1b[?1004h");
        assert!(core.snapshot(false, None).expect("snap").focus_reporting);
        assert_eq!(
            core.encode_focus(true).expect("focus in"),
            Some(b"\x1b[I".to_vec())
        );
        assert_eq!(
            core.encode_focus(false).expect("focus out"),
            Some(b"\x1b[O".to_vec())
        );
    }

    #[test]
    fn mouse_handling_reporting_vs_non_reporting() {
        let mut core = test_core(40, 10);
        for i in 0..20 {
            core.feed_bytes(format!("line-{i:02}\r\n").as_bytes());
        }

        let bottom = core.snapshot(false, None).expect("bottom");
        assert!(!bottom.mouse_tracking);
        let bottom_offset = bottom.scrollbar.viewport_offset;

        // With mouse tracking off, click/move are ignored and wheel scrolls scrollback.
        assert_eq!(
            core.handle_mouse(TermMouseEvent::press(
                TermMouseButton::Left,
                MousePosition::Cell { col: 2, row: 3 },
                TermModifiers::default(),
            ))
            .expect("click off"),
            MouseOutcome::Ignored
        );
        assert_eq!(
            core.handle_mouse(TermMouseEvent::wheel(
                0,
                3,
                MousePosition::Cell { col: 0, row: 0 },
                TermModifiers::default(),
            ))
            .expect("wheel up"),
            MouseOutcome::ScrolledViewport
        );
        let scrolled = core.snapshot(false, None).expect("scrolled");
        assert_eq!(scrolled.scrollbar.viewport_offset, bottom_offset - 3);

        // Alternate screen with mouse tracking off sends ArrowUp/ArrowDown for wheel.
        core.feed_bytes(b"\x1b[?1049h");
        assert_eq!(
            core.handle_mouse(TermMouseEvent::wheel(
                0,
                2,
                MousePosition::Cell { col: 0, row: 0 },
                TermModifiers::default(),
            ))
            .expect("alt wheel"),
            MouseOutcome::Reported(b"\x1b[A\x1b[A".to_vec())
        );

        // Enable any-event mouse tracking (1003) + SGR format (1006).
        core.feed_bytes(b"\x1b[?1003h\x1b[?1006h");
        assert!(core.snapshot(false, None).expect("snap").mouse_tracking);

        let press = core
            .handle_mouse(TermMouseEvent::press(
                TermMouseButton::Left,
                MousePosition::Cell { col: 4, row: 2 },
                TermModifiers::default(),
            ))
            .expect("sgr press");
        assert_eq!(press, MouseOutcome::Reported(b"\x1b[<0;5;3M".to_vec()));

        // Motion to cell (5, 2) reports; repeat motion inside same cell (5, 2) is deduplicated.
        let move1 = core
            .handle_mouse(TermMouseEvent::motion(
                Some(TermMouseButton::Left),
                MousePosition::Cell { col: 5, row: 2 },
                TermModifiers::default(),
            ))
            .expect("sgr move");
        assert_eq!(move1, MouseOutcome::Reported(b"\x1b[<32;6;3M".to_vec()));

        let move_same_cell = core
            .handle_mouse(TermMouseEvent::motion(
                Some(TermMouseButton::Left),
                MousePosition::Cell { col: 5, row: 2 },
                TermModifiers::default(),
            ))
            .expect("sgr move dedupe");
        assert_eq!(move_same_cell, MouseOutcome::Ignored);

        let release = core
            .handle_mouse(TermMouseEvent::release(
                TermMouseButton::Left,
                MousePosition::Cell { col: 5, row: 2 },
                TermModifiers::default(),
            ))
            .expect("sgr release");
        assert_eq!(release, MouseOutcome::Reported(b"\x1b[<0;6;3m".to_vec()));

        let wheel = core
            .handle_mouse(TermMouseEvent::wheel(
                0,
                -1,
                MousePosition::Cell { col: 1, row: 1 },
                TermModifiers::default(),
            ))
            .expect("sgr wheel down");
        assert_eq!(wheel, MouseOutcome::Reported(b"\x1b[<65;2;2M".to_vec()));
    }

    #[test]
    fn dirty_tracking_rebuilds_only_changed_rows_and_coalesces() {
        let mut core = test_core(30, 8);
        let initial = core.poll_snapshot(false, None).expect("initial");
        assert!(initial.is_some());
        assert_eq!(core.last_dirty_rows_rebuilt(), 8);

        // No further changes -> poll_snapshot returns None and rebuilds 0 rows.
        assert_eq!(core.poll_snapshot(false, None).expect("unchanged"), None);
        assert_eq!(core.last_dirty_rows_rebuilt(), 0);

        // Writing a single character on row 0 should mark only 1 row dirty.
        core.feed_bytes(b"X");
        let updated = core
            .poll_snapshot(false, None)
            .expect("single row")
            .expect("published");
        assert_eq!(core.last_dirty_rows_rebuilt(), 1);
        assert_eq!(updated.row_text(0), "X");
    }

    #[test]
    fn vt_theme_change_updates_colors_and_snapshot() {
        let mut core = test_core(20, 4);
        core.feed_bytes(b"\x1b[31mred\x1b[0m");
        let before = core.poll_snapshot(false, None).expect("before").expect("s");
        assert_eq!(before.grid[0].cells[0].fg, before.palette[1]);

        let mut custom_palette = before.palette;
        custom_palette[1] = Rgb::new(0xFA, 0x10, 0x20);
        let new_theme = TerminalTheme {
            foreground: Rgb::new(0x11, 0x22, 0x33),
            background: Rgb::new(0xF8, 0xFA, 0xFC),
            cursor: Some(Rgb::new(0x01, 0x02, 0x03)),
            palette: Some(custom_palette),
        };
        core.set_theme(&new_theme).expect("set_theme");

        let after = core
            .poll_snapshot(false, None)
            .expect("after")
            .expect("theme change must yield new snapshot");
        assert_eq!(after.default_fg, Rgb::new(0x11, 0x22, 0x33));
        assert_eq!(after.default_bg, Rgb::new(0xF8, 0xFA, 0xFC));
        assert_eq!(
            after.explicit_cursor_color,
            Some(Rgb::new(0x01, 0x02, 0x03))
        );
        assert_eq!(after.palette[1], Rgb::new(0xFA, 0x10, 0x20));
        assert_eq!(after.grid[0].cells[0].fg, Rgb::new(0xFA, 0x10, 0x20));
    }

    #[test]
    fn pty_theme_change_emits_snapshot_event() {
        let options = TerminalOptions {
            cols: 40,
            rows: 10,
            ..TerminalOptions::default()
        };
        let (session, events) = TerminalSession::spawn(options).expect("spawn");
        while events.try_recv().is_ok() {}

        let new_bg = Rgb::new(0xFE, 0xFD, 0xFC);
        let new_fg = Rgb::new(0x12, 0x13, 0x14);
        session.set_theme(TerminalTheme {
            foreground: new_fg,
            background: new_bg,
            cursor: Some(Rgb::new(0xAA, 0xBB, 0xCC)),
            palette: None,
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_event = false;
        while Instant::now() < deadline {
            if let Ok(ev) = events.recv_timeout(Duration::from_millis(50))
                && ev == TerminalEvent::SnapshotChanged
            {
                let snap = session.snapshot();
                if snap.default_bg == new_bg && snap.default_fg == new_fg {
                    saw_event = true;
                    break;
                }
            }
        }
        assert!(
            saw_event,
            "expected SnapshotChanged event with updated theme"
        );
    }

    #[test]
    fn pty_shell_printf_hello_and_resize() {
        let options = TerminalOptions {
            cols: 60,
            rows: 16,
            ..TerminalOptions::default()
        };
        let (session, _events) = TerminalSession::spawn(options).expect("spawn shell via PTY");

        #[cfg(windows)]
        session.write_str("echo hello\r\n");
        #[cfg(not(windows))]
        session.write_str("printf 'hello\\n'\n");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_hello = false;
        while Instant::now() < deadline {
            let snap = session.snapshot();
            if snap.text().lines().any(|line| line.contains("hello")) {
                saw_hello = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(saw_hello, "timed out waiting for `hello` in PTY snapshot");

        session.resize(90, 24, 9, 18);
        let resize_deadline = Instant::now() + Duration::from_secs(2);
        let mut resized = false;
        while Instant::now() < resize_deadline {
            let snap = session.snapshot();
            if snap.cols == 90 && snap.rows == 24 {
                resized = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(resized, "timed out waiting for resize snapshot update");

        session.write_str("exit\n");
    }

    #[cfg(not(windows))]
    #[test]
    fn pty_env_title_bell_and_child_exit_events() {
        let mut options = sh_options(
            60,
            10,
            "printf '\\033]0;MyTitle\\007\\007env=%s:%s:%s\\n' \"$TERM\" \"$COLORTERM\" \"$CUSTOM_VAR\"; exit 42",
        );
        options.env = vec![("CUSTOM_VAR".to_string(), "ok".to_string())];

        let (session, events) = TerminalSession::spawn(options).expect("spawn /bin/sh");

        let mut saw_bell = false;
        let mut saw_title = None;
        let mut exited_status = None;
        let deadline = Instant::now() + Duration::from_secs(5);

        while Instant::now() < deadline {
            match events.recv_timeout(Duration::from_millis(50)) {
                Ok(TerminalEvent::Bell) => saw_bell = true,
                Ok(TerminalEvent::TitleChanged(t)) => saw_title = Some(t),
                Ok(TerminalEvent::ChildExited(st)) => {
                    exited_status = Some(st);
                    break;
                }
                Ok(TerminalEvent::SnapshotChanged) => {}
                Ok(TerminalEvent::ResizeFailed(error)) => {
                    panic!("unexpected resize failure: {error}")
                }
                Err(_) => {}
            }
        }

        assert!(saw_bell, "expected Bell event");
        assert_eq!(saw_title.as_deref(), Some("MyTitle"));

        let snap = session.snapshot();
        assert!(snap.exited);
        assert!(
            snap.text().contains("env=xterm-256color:truecolor:ok"),
            "unexpected snapshot text: {:?}",
            snap.text()
        );
        let status = exited_status
            .expect("expected ChildExited event")
            .expect("expected OS ExitStatus");
        assert_eq!(status.code, 42);
        assert!(!status.success());
    }

    #[cfg(not(windows))]
    #[test]
    fn pty_mouse_reporting_end_to_end() {
        let options = sh_options(
            40,
            10,
            "stty raw -echo; printf '\\033[?1000;1006hREADY\\n'; dd bs=9 count=1 2>/dev/null | tr '\\033' 'E'; printf '\\n'",
        );
        let (session, _events) = TerminalSession::spawn(options).expect("spawn mouse probe");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        while Instant::now() < deadline {
            let snap = session.snapshot();
            if snap.mouse_tracking && snap.text().contains("READY") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(ready, "timed out waiting for mouse tracking to activate");

        session.handle_mouse(TermMouseEvent::press(
            TermMouseButton::Left,
            MousePosition::Cell { col: 3, row: 4 },
            TermModifiers::default(),
        ));

        let mut saw_seq = false;
        while Instant::now() < deadline {
            if session.snapshot().text().contains("E[<0;4;5M") {
                saw_seq = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(saw_seq, "expected SGR mouse press sequence echoed by PTY");
    }

    #[cfg(not(windows))]
    #[test]
    fn startup_output_survives_the_spawn_handshake() {
        // `spawn` used to write its own initial snapshot after the actor had
        // already published one built from early PTY output. That replaced the
        // grid with an empty one, and since the core's dirty state was already
        // consumed, the output never came back.
        for round in 0..3 {
            let options = sh_options(40, 10, "printf 'MARK\\n'; sleep 60");
            let (session, _events) = TerminalSession::spawn(options).expect("spawn");

            // `spawn` only returns once a snapshot is published.
            let first = session.snapshot();
            assert_eq!((first.cols, first.rows), (40, 10), "round {round}");

            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && !session.snapshot().text().contains("MARK") {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                session.snapshot().text().contains("MARK"),
                "round {round}: output printed at startup was lost"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn resize_sent_right_after_spawn_survives_console_startup() {
        // ConPTY drops a resize issued while the console client is still
        // starting and reports success anyway, so the actor re-sends the size
        // once the first output arrives. The shell must end up reporting the
        // grid from after the resize, not the one it was spawned with.
        let options = sh_options(80, 16, "sleep 0.3; stty size");
        let (session, events) = TerminalSession::spawn(options).expect("spawn sleeping shell");

        // Sent immediately, while the shell is still starting.
        session.resize(100, 30, 8, 16);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for the shell to exit"
            );
            match events.recv_timeout(remaining) {
                Ok(TerminalEvent::ChildExited(_)) => break,
                Ok(_) => {}
                Err(err) => panic!("terminal events stopped before the shell exited: {err}"),
            }
        }

        let text = session.snapshot().text();
        assert!(
            text.contains("30 100"),
            "shell must see the resized grid, got: {text:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn drop_terminates_child_process_and_leaves_no_zombie() {
        let options = sh_options(40, 10, "printf 'ALIVE\\n'; sleep 60");
        let (session, _events) = TerminalSession::spawn(options).expect("spawn sleeping shell");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut alive = false;
        while Instant::now() < deadline {
            if session.snapshot().text().contains("ALIVE") {
                alive = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(alive, "shell never printed ALIVE");

        let pid = session.child_pid().expect("child_pid on Unix") as libc::pid_t;
        assert!(pid > 0);
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "child should still run before drop"
        );

        drop(session);

        // After drop returns, actor has killed + waited child, so ESRCH is immediate
        // (and waitpid(WNOHANG) would confirm it is not left as a zombie).
        let rc = unsafe { libc::kill(pid, 0) };
        let err = std::io::Error::last_os_error();
        assert_eq!(rc, -1, "child process {pid} survived drop");
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ESRCH),
            "expected ESRCH (no zombie or live pid), got {err:?}"
        );
    }
}
