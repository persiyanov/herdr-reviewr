//! The console's VT byte stream parsed into crossterm's unix events, via `terminput`.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::Event;

use crate::wake::Woke;

/// One console record the reader takes: a key's UTF-16 unit, or a resize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Record {
    Unit(u16),
    Resize(u16, u16),
}

/// The console the reader polls: its clock, a wait for input or a wake, and a read of what is queued.
pub(super) trait Console {
    fn now(&self) -> Instant;
    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Woke>;
    fn read(&mut self) -> io::Result<Vec<Record>>;
}

/// The parser state between console reads.
#[derive(Debug, Default)]
pub(super) struct VtInput {
    /// The bytes of a sequence that has started but not finished.
    pending: Vec<u8>,
    /// A high surrogate whose low half is still to come.
    surrogate: Option<u16>,
    /// Parsed events, oldest first.
    events: VecDeque<Event>,
}

impl VtInput {
    /// Take one UTF-16 unit from a key record.
    fn feed(&mut self, unit: u16) {
        let ch = match unit {
            0xD800..=0xDBFF => {
                self.surrogate = Some(unit);
                return;
            }
            0xDC00..=0xDFFF => {
                let Some(high) = self.surrogate.take() else { return };
                char::decode_utf16([high, unit]).next().and_then(Result::ok)
            }
            _ => {
                self.surrogate = None;
                char::from_u32(u32::from(unit))
            }
        };
        let mut utf8 = [0; 4];
        for &byte in ch.unwrap_or(char::REPLACEMENT_CHARACTER).encode_utf8(&mut utf8).as_bytes() {
            self.pending.push(byte);
            self.parse(true);
        }
    }

    /// Nothing more is queued: a lone ESC is the Esc key.
    fn settle(&mut self) {
        self.parse(false);
    }

    /// Read `console` until an event parses (`Input`), a wake comes, or `deadline` passes; an open
    /// paste waits for its end, its bytes kept here across a wake.
    pub(super) fn poll(
        &mut self,
        console: &mut impl Console,
        deadline: Option<Instant>,
    ) -> io::Result<Woke> {
        loop {
            // Everything queued is read, then settled once, so a split sequence stays whole.
            let mut fed = false;
            let mut woken = false;
            loop {
                match console.wait(Some(Duration::ZERO))? {
                    Woke::Input => fed |= self.take(console.read()?),
                    Woke::Woken => woken = true,
                    Woke::Deadline => break,
                }
            }
            if fed {
                self.settle();
            }
            if !self.events.is_empty() {
                return Ok(Woke::Input);
            }
            if woken {
                return Ok(Woke::Woken);
            }
            let left = deadline.map(|deadline| deadline.saturating_duration_since(console.now()));
            match console.wait(left)? {
                Woke::Input => {}
                other => return Ok(other),
            }
        }
    }

    /// Feed one read's records; whether any carried a byte.
    fn take(&mut self, records: Vec<Record>) -> bool {
        let mut fed = false;
        for record in records {
            match record {
                Record::Unit(unit) => {
                    self.feed(unit);
                    fed = true;
                }
                Record::Resize(cols, rows) => self.events.push_back(Event::Resize(cols, rows)),
            }
        }
        fed
    }

    pub(super) fn pop(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Try the pending bytes as one event, the step crossterm's unix reader runs per byte.
    fn parse(&mut self, more: bool) {
        // `parse_from` would read a lone ESC as Esc, so wait while more is queued.
        if more && self.pending == b"\x1b" {
            return;
        }
        match terminput::Event::parse_from(&self.pending) {
            Ok(Some(event)) => {
                // crossterm drops what it has no type for, and so does this.
                if let Ok(event) = terminput_crossterm::to_crossterm(event) {
                    self.events.push_back(event);
                }
                self.pending.clear();
            }
            Ok(None) => {}
            Err(_) => self.pending.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    /// Feed each batch as one console read and collect every event, in order.
    fn events(vt: &mut VtInput, batches: &[&str]) -> Vec<Event> {
        for batch in batches {
            for unit in batch.encode_utf16() {
                vt.feed(unit);
            }
            vt.settle();
        }
        drain(vt)
    }

    fn parse(batches: &[&str]) -> Vec<Event> {
        events(&mut VtInput::default(), batches)
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })
    }

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const CONTROL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;

    #[test]
    fn keys_map_to_the_events_crossterm_reads_on_unix() {
        // The bytes herdr's ConPTY delivered per key: legacy, then disambiguated.
        let rows: &[(&str, Event)] = &[
            ("a", key(KeyCode::Char('a'), NONE)),
            ("A", key(KeyCode::Char('A'), SHIFT)),
            ("é", key(KeyCode::Char('é'), NONE)),
            ("日", key(KeyCode::Char('日'), NONE)),
            ("🙂", key(KeyCode::Char('🙂'), NONE)),
            ("\r", key(KeyCode::Enter, NONE)),
            ("\n", key(KeyCode::Char('j'), CONTROL)),
            ("\x7f", key(KeyCode::Backspace, NONE)),
            ("\x1b", key(KeyCode::Esc, NONE)),
            ("\x1b\r", key(KeyCode::Enter, ALT)),
            ("\x1bx", key(KeyCode::Char('x'), ALT)),
            ("\x1b\x7f", key(KeyCode::Backspace, ALT)),
            ("\x1b[3;3~", key(KeyCode::Delete, ALT)),
            ("\x1b[A", key(KeyCode::Up, NONE)),
            ("\x1b[D", key(KeyCode::Left, NONE)),
            ("\x1b[1;5D", key(KeyCode::Left, CONTROL)),
            ("\x1b[Z", key(KeyCode::BackTab, SHIFT)),
            ("\x1b[13;2u", key(KeyCode::Enter, SHIFT)),
            ("\x1b[13;3u", key(KeyCode::Enter, ALT)),
            ("\x1b[106;5u", key(KeyCode::Char('j'), CONTROL)),
            ("\x1b[27u", key(KeyCode::Esc, NONE)),
            ("\x1b[120;3u", key(KeyCode::Char('x'), ALT)),
            ("\x1b[9;2u", key(KeyCode::BackTab, SHIFT)),
        ];
        for (bytes, expected) in rows {
            assert_eq!(parse(&[bytes]), vec![expected.clone()], "{bytes:?}");
        }
    }

    #[test]
    fn sgr_mouse_reports_map_to_zero_based_mouse_events() {
        let rows: &[(&str, Event)] = &[
            ("\x1b[<0;10;5M", mouse(MouseEventKind::Down(MouseButton::Left), 9, 4)),
            ("\x1b[<0;10;5m", mouse(MouseEventKind::Up(MouseButton::Left), 9, 4)),
            ("\x1b[<32;11;5M", mouse(MouseEventKind::Drag(MouseButton::Left), 10, 4)),
            ("\x1b[<64;10;5M", mouse(MouseEventKind::ScrollUp, 9, 4)),
            ("\x1b[<65;10;5M", mouse(MouseEventKind::ScrollDown, 9, 4)),
        ];
        for (bytes, expected) in rows {
            assert_eq!(parse(&[bytes]), vec![expected.clone()], "{bytes:?}");
        }
    }

    #[test]
    fn a_bracketed_paste_is_one_event_with_its_text_verbatim() {
        // Exactly what herdr's paste handler writes on Windows: markers, CRLF line breaks.
        let pasted = "\x1b[200~line one\r\nline two é 日本 🙂\x1b[201~";
        assert_eq!(parse(&[pasted]), vec![Event::Paste("line one\r\nline two é 日本 🙂".into())]);
        // Split across console reads, it is still one paste.
        assert_eq!(
            parse(&["\x1b[200~line one\r", "\nline two\x1b[2", "01~"]),
            vec![Event::Paste("line one\r\nline two".into())]
        );
    }

    #[test]
    fn a_sequence_split_across_reads_waits_for_its_end() {
        assert_eq!(parse(&["\x1b[", "A"]), vec![key(KeyCode::Up, NONE)]);
        // A surrogate pair split across reads is one character.
        let smile: Vec<u16> = "🙂".encode_utf16().collect();
        let mut vt = VtInput::default();
        vt.feed(smile[0]);
        vt.settle();
        vt.feed(smile[1]);
        vt.settle();
        assert_eq!(vt.pop(), Some(key(KeyCode::Char('🙂'), NONE)));
    }

    #[test]
    fn a_lone_esc_at_the_end_of_a_read_is_the_esc_key() {
        assert_eq!(
            parse(&["\x1b", "j"]),
            vec![key(KeyCode::Esc, NONE), key(KeyCode::Char('j'), NONE)]
        );
    }

    /// A console on a test-driven clock; each batch becomes readable at its own time, and a wake
    /// fires at its own.
    struct FakeConsole {
        now: Instant,
        batches: VecDeque<(Instant, Vec<Record>)>,
        wakes: VecDeque<Instant>,
    }

    impl FakeConsole {
        fn new() -> Self {
            Self { now: Instant::now(), batches: VecDeque::new(), wakes: VecDeque::new() }
        }

        /// Queue `text` to arrive `after` from now, as one read.
        fn queue(&mut self, after: Duration, text: &str) {
            let units = text.encode_utf16().map(Record::Unit).collect();
            self.batches.push_back((self.now + after, units));
        }
    }

    impl Console for FakeConsole {
        fn now(&self) -> Instant {
            self.now
        }

        fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Woke> {
            let limit = timeout.map(|t| self.now + t);
            let due = |at: &Instant| limit.is_none_or(|limit| *at <= limit);
            let input = self.batches.front().map(|(at, _)| *at).filter(due);
            let wake = self.wakes.front().copied().filter(due);
            match (input, wake) {
                // Input first, as `WaitForMultipleObjects` reports the lowest signaled handle.
                (Some(at), w) if w.is_none_or(|w| at <= w) => {
                    self.now = self.now.max(at);
                    Ok(Woke::Input)
                }
                (_, Some(at)) => {
                    self.wakes.pop_front();
                    self.now = self.now.max(at);
                    Ok(Woke::Woken)
                }
                _ => {
                    self.now = limit.expect("an endless wait with nothing queued");
                    Ok(Woke::Deadline)
                }
            }
        }

        fn read(&mut self) -> io::Result<Vec<Record>> {
            Ok(self.batches.pop_front().map(|(_, units)| units).unwrap_or_default())
        }
    }

    fn drain(vt: &mut VtInput) -> Vec<Event> {
        std::iter::from_fn(|| vt.pop()).collect()
    }

    #[test]
    fn a_paste_stalled_mid_way_never_runs_its_rest_as_keys() {
        let (mut vt, mut console) = (VtInput::default(), FakeConsole::new());
        console.queue(Duration::ZERO, "\x1b[200~abc\r");
        console.queue(Duration::from_secs(5), "\nx\x1b[201~");
        let tick = Some(console.now + Duration::from_secs(1));
        assert_eq!(
            vt.poll(&mut console, tick).unwrap(),
            Woke::Deadline,
            "an open paste waits for its end"
        );
        assert_eq!(vt.poll(&mut console, None).unwrap(), Woke::Input);
        assert_eq!(drain(&mut vt), [Event::Paste("abc\r\nx".into())]);
    }

    #[test]
    fn a_frames_draw_time_never_closes_an_open_paste() {
        let (mut vt, mut console) = (VtInput::default(), FakeConsole::new());
        console.queue(Duration::ZERO, "\x1b[200~ab");
        let at = Some(console.now);
        assert_eq!(vt.poll(&mut console, at).unwrap(), Woke::Deadline);
        // A second's draw between polls, the rest of the paste queued meanwhile.
        console.now += Duration::from_secs(1);
        console.queue(Duration::ZERO, "cd\x1b[201~");
        let at = Some(console.now);
        assert_eq!(vt.poll(&mut console, at).unwrap(), Woke::Input);
        assert_eq!(drain(&mut vt), [Event::Paste("abcd".into())]);
    }

    #[test]
    fn a_sequence_split_across_queued_reads_is_one_key() {
        let (mut vt, mut console) = (VtInput::default(), FakeConsole::new());
        console.queue(Duration::ZERO, "\x1b");
        console.queue(Duration::ZERO, "[A");
        let at = Some(console.now);
        assert_eq!(vt.poll(&mut console, at).unwrap(), Woke::Input);
        assert_eq!(drain(&mut vt), [key(KeyCode::Up, NONE)]);
    }

    #[test]
    fn a_wake_ends_the_wait_and_an_open_paste_survives_it() {
        let (mut vt, mut console) = (VtInput::default(), FakeConsole::new());
        console.queue(Duration::ZERO, "\x1b[200~ab");
        console.wakes.push_back(console.now + Duration::from_millis(10));
        console.queue(Duration::from_millis(20), "cd\x1b[201~");
        assert_eq!(vt.poll(&mut console, None).unwrap(), Woke::Woken, "a worker's wake ends it");
        assert_eq!(vt.poll(&mut console, None).unwrap(), Woke::Input);
        assert_eq!(drain(&mut vt), [Event::Paste("abcd".into())], "the paste stays whole");
    }

    #[test]
    fn input_and_a_wake_together_read_the_input_first() {
        let (mut vt, mut console) = (VtInput::default(), FakeConsole::new());
        console.queue(Duration::ZERO, "j");
        console.wakes.push_back(console.now);
        // Input answers for both: after any return the loop drains every worker's channel.
        assert_eq!(vt.poll(&mut console, None).unwrap(), Woke::Input);
        assert_eq!(drain(&mut vt), [key(KeyCode::Char('j'), NONE)]);
    }
}
