//! Shared VT event parsing via `terminput`, with Unix capability replies framed before events.

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
    #[cfg(not(windows))]
    state: Frame,
    #[cfg(not(windows))]
    deadline: Option<Instant>,
    #[cfg(not(windows))]
    replies: VecDeque<Vec<u8>>,
    #[cfg(not(windows))]
    keyboard_query: Option<Instant>,
    #[cfg(not(windows))]
    keyboard_push: bool,
    #[cfg(not(windows))]
    keyboard_enabled: bool,
    #[cfg(not(windows))]
    stalled: bool,
}

impl VtInput {
    /// Take one UTF-16 unit from a key record.
    fn feed_unit(&mut self, unit: u16) {
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
    pub(crate) fn settle(&mut self) {
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
                    self.feed_unit(unit);
                    fed = true;
                }
                Record::Resize(cols, rows) => self.events.push_back(Event::Resize(cols, rows)),
            }
        }
        fed
    }

    pub(crate) fn pop(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    #[cfg(not(windows))]
    pub(crate) fn has_event(&self) -> bool {
        !self.events.is_empty()
    }

    /// Raw Unix bytes retain incomplete UTF-8 without consuming the next character on error.
    #[cfg(not(windows))]
    pub(crate) fn feed_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.pending.first().is_some_and(|b| *b >= 0xc0) && byte & 0xc0 != 0x80 {
                self.pending.clear();
            }
            self.pending.push(byte);
            self.parse(true);
        }
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

#[cfg(not(windows))]
#[derive(Debug, Default, PartialEq, Eq)]
enum Frame {
    #[default]
    Ground,
    Escape,
    ControlPrefix,
    Csi {
        malformed: bool,
        oversized: bool,
    },
    String {
        escaped: bool,
        oversized: bool,
    },
    Paste,
    DrainString {
        escaped: bool,
    },
    DrainCsi {
        escaped: bool,
        malformed: bool,
    },
}

#[cfg(not(windows))]
impl VtInput {
    const ESC_GRACE: Duration = Duration::from_millis(35);
    const ENVELOPE_GRACE: Duration = Duration::from_millis(250);
    const MAX_REPLY: usize = 4096;
    const MAX_REPLIES: usize = 32;

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn discarded_handoff(&self) -> Option<Self> {
        let state = match self.state {
            Frame::String { escaped, .. } | Frame::DrainString { escaped } => {
                Frame::DrainString { escaped }
            }
            Frame::DrainCsi { malformed, .. } => Frame::DrainCsi { escaped: false, malformed },
            Frame::Csi { malformed, .. }
                if self.pending.get(2) == Some(&b'?')
                    || self.pending.starts_with(b"\x1b[6;")
                    || self.pending.starts_with(b"\x1b[4;") =>
            {
                Frame::DrainCsi { escaped: false, malformed }
            }
            _ => return None,
        };
        Some(Self { state, stalled: true, ..Self::default() })
    }

    pub(super) fn query_keyboard(&mut self, now: Instant) {
        self.keyboard_query = Some(now + Duration::from_secs(1));
    }

    pub(super) fn keyboard_enabled(&self) -> bool {
        self.keyboard_enabled
    }

    pub(super) fn take_keyboard_push(&mut self) -> bool {
        std::mem::take(&mut self.keyboard_push)
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline.into_iter().chain(self.keyboard_query).min()
    }

    pub(super) fn expire(&mut self, now: Instant) {
        if self.keyboard_query.is_some_and(|at| at <= now) {
            self.keyboard_query = None;
        }
        if self.deadline.is_some_and(|at| at <= now) {
            let draining = matches!(self.state, Frame::String { .. });
            let escaped = matches!(self.state, Frame::String { escaped: true, .. });
            let csi_reply = matches!(self.state, Frame::Csi { .. })
                && (self.pending.get(2) == Some(&b'?')
                    || self.pending.starts_with(b"\x1b[6;")
                    || self.pending.starts_with(b"\x1b[4;"));
            let malformed = matches!(self.state, Frame::Csi { malformed: true, .. });
            if self.state == Frame::Escape
                || matches!(
                    self.state,
                    Frame::DrainString { escaped: true } | Frame::DrainCsi { escaped: true, .. }
                )
            {
                self.pending.clear();
                self.pending.push(0x1b);
                self.settle();
            }
            if matches!(self.state, Frame::ControlPrefix) {
                self.settle();
            }
            if self.pending == b"\x1b[" {
                use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
                self.events
                    .push_back(Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::ALT)));
            }
            if draining || csi_reply {
                self.stalled = true;
            }
            self.pending.clear();
            self.state = if draining {
                Frame::DrainString { escaped }
            } else if csi_reply {
                Frame::DrainCsi { escaped: false, malformed }
            } else {
                self.stalled = false;
                Frame::Ground
            };
            self.deadline = None;
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8], now: Instant) {
        self.expire(now);
        for &byte in bytes {
            self.byte(byte, now);
        }
    }

    fn byte(&mut self, byte: u8, now: Instant) {
        match &mut self.state {
            Frame::Ground => {
                if byte == 0x1b {
                    self.pending.clear();
                    self.pending.push(byte);
                    self.state = Frame::Escape;
                    self.deadline = Some(now + Self::ESC_GRACE);
                } else {
                    self.feed_bytes(&[byte]);
                    self.deadline =
                        (!self.pending.is_empty()).then_some(now + Self::ENVELOPE_GRACE);
                }
            }
            Frame::Escape => {
                if byte == 0x1b {
                    self.settle();
                    self.pending.clear();
                    self.pending.push(byte);
                    self.deadline = Some(now + Self::ESC_GRACE);
                    return;
                }
                self.pending.push(byte);
                self.deadline = Some(now + Self::ENVELOPE_GRACE);
                match byte {
                    b'[' => self.state = Frame::Csi { malformed: false, oversized: false },
                    b'_' | b'P' | b']' | b'^' | b'X' => {
                        self.state = Frame::ControlPrefix;
                        self.deadline = Some(now + Self::ESC_GRACE);
                    }
                    _ => {
                        self.parse(true);
                        self.state = Frame::Ground;
                        self.deadline =
                            (!self.pending.is_empty()).then_some(now + Self::ENVELOPE_GRACE);
                    }
                }
            }
            Frame::ControlPrefix => {
                if self.pending.get(1) == Some(&b'_') && byte == b'G' {
                    self.pending.push(byte);
                    self.state = Frame::String { escaped: false, oversized: false };
                    self.deadline = Some(now + Self::ENVELOPE_GRACE);
                } else {
                    self.settle();
                    self.finish();
                    self.byte(byte, now);
                }
            }
            Frame::DrainCsi { escaped, malformed } => {
                if *escaped {
                    self.finish();
                    self.byte(0x1b, now);
                    self.byte(byte, now);
                } else if byte == 0x1b {
                    *escaped = true;
                    self.deadline = Some(now + Self::ESC_GRACE);
                } else if !*malformed && matches!(byte, b'c' | b't' | b'u' | b'n') {
                    self.finish();
                } else if !(byte.is_ascii_digit() || matches!(byte, b';' | b':' | b'?' | b'>')) {
                    *malformed = true;
                }
            }
            Frame::DrainString { escaped } => {
                if *escaped && byte == b'\\' {
                    self.finish();
                } else if *escaped {
                    self.finish();
                    self.byte(0x1b, now);
                    self.byte(byte, now);
                } else if byte == 0x1b {
                    *escaped = true;
                    self.deadline = Some(now + Self::ESC_GRACE);
                }
            }
            Frame::Paste => {
                self.pending.push(byte);
                if self.pending.ends_with(b"\x1b[201~") {
                    self.parse(false);
                    self.finish();
                }
            }
            Frame::String { escaped, oversized } => {
                let end = *escaped && byte == b'\\';
                *escaped = byte == 0x1b;
                if self.pending.len() < Self::MAX_REPLY {
                    self.pending.push(byte);
                } else {
                    *oversized = true;
                }
                if end {
                    if !*oversized && valid_apc(&self.pending) {
                        self.queue_reply();
                    }
                    self.finish();
                }
            }
            Frame::Csi { malformed, oversized } => {
                if self.pending.len() < Self::MAX_REPLY {
                    self.pending.push(byte);
                } else {
                    *oversized = true;
                }
                // Linux's legacy F1-F5 prefix has a second '[' before the real final byte.
                if self.pending == b"\x1b[[" {
                    return;
                }
                if (0x40..=0x7e).contains(&byte) {
                    if *malformed {
                        return;
                    }
                    if *oversized {
                        // Oversized numeric replies drain through their final or expiry.
                        if matches!(byte, b'c' | b't' | b'u' | b'n' | b'~') {
                            self.finish();
                        }
                        return;
                    }
                    if self.pending.get(2) == Some(&b'?') && !matches!(byte, b'c' | b'u' | b'n') {
                        if self.pending[3..self.pending.len() - 1].iter().any(u8::is_ascii_digit) {
                            self.finish();
                        } else {
                            *malformed = true;
                        }
                        return;
                    }
                    if self.pending == b"\x1b[200~" {
                        self.state = Frame::Paste;
                        self.deadline = None;
                    } else {
                        if matches!(byte, b'c' | b't' | b'n')
                            || (byte == b'u' && self.pending.get(2) == Some(&b'?'))
                        {
                            self.keyboard_reply(now);
                            if valid_csi_reply(&self.pending) {
                                self.queue_reply();
                            }
                            self.finish();
                            return;
                        }
                        self.parse(false);
                        self.state = Frame::Ground;
                        self.deadline = None;
                        self.stalled = false;
                        if !self.pending.is_empty() {
                            self.deadline = Some(now + Self::ENVELOPE_GRACE);
                        }
                    }
                } else if !(0x20..=0x3f).contains(&byte) {
                    *malformed = true;
                }
            }
        }
    }

    fn finish(&mut self) {
        self.stalled = false;
        self.pending.clear();
        self.state = Frame::Ground;
        self.deadline = None;
    }

    fn queue_reply(&mut self) {
        if self.replies.len() == Self::MAX_REPLIES {
            self.replies.pop_front();
        }
        self.replies.push_back(self.pending.clone());
    }

    fn keyboard_reply(&mut self, now: Instant) {
        if self.pending.starts_with(b"\x1b[?")
            && self.pending.ends_with(b"u")
            && self.keyboard_query.is_some_and(|at| now < at)
            && std::str::from_utf8(&self.pending[3..self.pending.len() - 1])
                .ok()
                .and_then(|flags| flags.parse::<u8>().ok())
                .is_some()
        {
            self.keyboard_query = None;
            self.keyboard_enabled = true;
            self.keyboard_push = true;
        }
    }

    pub(super) fn has_reply(&self) -> bool {
        !self.replies.is_empty()
    }

    pub(super) fn stalled(&self) -> bool {
        self.stalled
    }

    pub(super) fn take_reply(&mut self) -> Option<Vec<u8>> {
        self.replies.pop_front()
    }

    pub(super) fn resize(&mut self, size: (u16, u16)) {
        self.events.push_back(Event::Resize(size.0, size.1));
    }
}

#[cfg(not(windows))]
fn valid_csi_reply(bytes: &[u8]) -> bool {
    let params = &bytes[2..bytes.len() - 1];
    let params = params.strip_prefix(b"?").or_else(|| params.strip_prefix(b">")).unwrap_or(params);
    !params.is_empty() && params.iter().all(|b| b.is_ascii_digit() || *b == b';' || *b == b':')
}

#[cfg(not(windows))]
fn valid_apc(bytes: &[u8]) -> bool {
    let Some(body) = bytes.strip_prefix(b"\x1b_G").and_then(|b| b.strip_suffix(b"\x1b\\")) else {
        return false;
    };
    let Some(separator) = body.iter().position(|b| *b == b';') else { return false };
    let (header, tail) = body.split_at(separator);
    let message = &tail[1..];
    !header.is_empty()
        && !message.is_empty()
        && header.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b',' | b'-'))
        && message.iter().all(|b| (0x20..=0x7e).contains(b))
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
                vt.feed_unit(unit);
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
        vt.feed_unit(smile[0]);
        vt.settle();
        vt.feed_unit(smile[1]);
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

#[cfg(all(test, not(windows)))]
mod terminal_tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};

    fn key(ch: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
    }

    fn drain(input: &mut VtInput) -> Vec<Event> {
        std::iter::from_fn(|| input.pop()).collect()
    }

    #[test]
    fn every_reply_split_is_owned_even_when_unsolicited_or_late() {
        let replies: &[&[u8]] = &[
            b"\x1b_Gi=27;OK\x1b\\",
            b"\x1b_Gi=999;ENOENT:not found\x1b\\",
            b"\x1b[4;600;800t",
            b"\x1b[6;20;10t",
            b"\x1b[8;30;80t",
            b"\x1b[?1;2c",
            b"\x1b[>1;4000;0c",
            b"\x1b[?0u",
            b"\x1b[0n",
        ];
        let now = Instant::now();
        for &reply in replies {
            for split in 0..=reply.len() {
                let mut input = VtInput::default();
                input.feed(&reply[..split], now);
                input.feed(&reply[split..], now + Duration::from_millis(1));
                input.feed(b"j", now + Duration::from_millis(2));
                assert_eq!(drain(&mut input), [key('j')], "{reply:?} split {split}");
                assert_eq!(input.take_reply().as_deref(), Some(reply));
                assert!(!input.keyboard_enabled());
            }
        }
    }

    #[test]
    fn esc_deadline_is_armed_once_and_expires_to_a_key() {
        let now = Instant::now();
        let mut input = VtInput::default();
        assert_eq!(input.deadline(), None);
        input.feed(b"\x1b", now);
        assert_eq!(input.deadline(), Some(now + VtInput::ESC_GRACE));
        input.expire(now + Duration::from_millis(34));
        assert!(!input.has_event());
        input.expire(now + VtInput::ESC_GRACE);
        assert_eq!(input.pop(), Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))));
        assert_eq!(input.deadline(), None);
        input.feed(b"j", now + Duration::from_secs(1));
        assert_eq!(input.pop(), Some(key('j')));
    }

    #[test]
    fn bare_csi_prefix_expiry_preserves_the_alt_bracket_chord() {
        let now = Instant::now();
        for delay in [0, 1, 5, 34] {
            let mut input = VtInput::default();
            input.feed(b"\x1b", now);
            let bracket_at = now + Duration::from_millis(delay);
            input.feed(b"[", bracket_at);
            let before_expiry =
                VtInput::ENVELOPE_GRACE.checked_sub(Duration::from_millis(1)).unwrap();
            input.expire(bracket_at + before_expiry);
            assert!(!input.has_event());
            input.expire(bracket_at + VtInput::ENVELOPE_GRACE);
            assert_eq!(
                drain(&mut input),
                [Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::ALT))]
            );
            assert_eq!(input.deadline(), None);
            assert!(!input.stalled());
            assert_eq!(input.take_reply(), None);
            input.feed(b"j", bracket_at + Duration::from_secs(1));
            assert_eq!(input.pop(), Some(key('j')));
        }
    }

    #[test]
    fn paste_owns_apc_csi_and_invalid_utf8_across_large_reads() {
        let now = Instant::now();
        let mut body = vec![b'x'; 20_000];
        body.splice(8180..8180, b"\x1b_Gi=27;OK\x1b\\\x1b[?0u\x1b[6;20;10t\xff".iter().copied());
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(b"\x1b[201~j");
        for split in 0..=6 {
            let mut input = VtInput::default();
            input.feed(&bytes[..split], now);
            for chunk in bytes[split..].chunks(8192) {
                input.feed(chunk, now + Duration::from_millis(1));
            }
            assert_eq!(
                drain(&mut input),
                [Event::Paste(String::from_utf8_lossy(&body).into()), key('j')]
            );
            assert_eq!(input.take_reply(), None);
        }
        for split in 0..=6 {
            let mut input = VtInput::default();
            input.feed(b"\x1b[200~\x1b_", now);
            assert_eq!(input.deadline(), None);
            input.feed(b"Gi=27;OK\x1b\\", now + Duration::from_mins(1));
            input.feed(&b"\x1b[201~"[..split], now + Duration::from_secs(61));
            input.feed(&b"\x1b[201~"[split..], now + Duration::from_secs(62));
            assert_eq!(input.pop(), Some(Event::Paste("\x1b_Gi=27;OK\x1b\\".into())));
            assert_eq!(input.take_reply(), None);
        }
    }

    #[test]
    fn malformed_and_oversized_envelopes_discard_without_keys() {
        let mut oversized_apc = b"\x1b_Gi=27;".to_vec();
        oversized_apc.extend(vec![b'x'; 10_000]);
        oversized_apc.extend_from_slice(b"\x1b\\");
        let mut oversized_csi = b"\x1b[6;".to_vec();
        oversized_csi.extend(vec![b'1'; 10_000]);
        oversized_csi.push(b't');
        let cases: &[&[u8]] = &[
            &oversized_apc,
            &oversized_csi,
            b"\x1b_Gno header\x1b\\",
            b"\x1b_Gi=2;\xff\x1b\\",
            b"\x1b[?bad replyu",
            b"\x1b[6;\xff;2t",
        ];
        let now = Instant::now();
        for &bytes in cases {
            let mut input = VtInput::default();
            for byte in bytes {
                input.feed(&[*byte], now);
                assert!(input.pending.len() <= VtInput::MAX_REPLY);
            }
            input.expire(now + VtInput::ENVELOPE_GRACE);
            if input.stalled() {
                input.feed(b"\x1b[A", now + VtInput::ENVELOPE_GRACE);
                let _ = drain(&mut input);
            }
            input.feed(b"j", now + VtInput::ENVELOPE_GRACE);
            assert_eq!(drain(&mut input), [key('j')], "{bytes:?}");
            assert_eq!(input.take_reply(), None);
        }
    }

    #[test]
    fn incomplete_envelopes_have_finite_discard_expiry() {
        let now = Instant::now();
        for bytes in [b"\x1b_G".as_slice(), b"\x1b_Gi=12;no end", b"\x1b[?12", b"\x1b[6;12"] {
            let mut input = VtInput::default();
            input.feed(bytes, now);
            assert_eq!(input.deadline(), Some(now + VtInput::ENVELOPE_GRACE));
            input.expire(now + VtInput::ENVELOPE_GRACE);
            assert_eq!(input.deadline(), None);
            assert!(!input.has_event());
            assert!(input.pending.is_empty());
            if bytes.starts_with(b"\x1b_") {
                input.feed(b"Gi=12;OK\x1b\\j", now + Duration::from_secs(1));
            } else {
                input.feed(
                    if bytes.get(2) == Some(&b'?') { b"u" } else { b"t" },
                    now + Duration::from_secs(1),
                );
                input.feed(b"j", now + Duration::from_secs(1));
            }
            assert_eq!(input.pop(), Some(key('j')));
        }
    }

    #[test]
    fn a_recognized_reply_keeps_late_suffixes_quarantined_and_escape_recovers() {
        let now = Instant::now();
        let reply = b"\x1b_Gi=123;OK\x1b\\";
        for split in 3..reply.len() {
            let mut input = VtInput::default();
            input.feed(&reply[..split], now);
            input.expire(now + VtInput::ENVELOPE_GRACE);
            input.feed(&reply[split..], now + Duration::from_secs(2));
            assert!(!input.has_event(), "split {split}");
            input.feed(b"j", now + Duration::from_secs(2));
            assert_eq!(input.pop(), Some(key('j')));
        }
        let mut input = VtInput::default();
        input.feed(b"\x1b_Gnever terminated", now);
        input.expire(now + VtInput::ENVELOPE_GRACE);
        input.feed(b"\x1b", now + Duration::from_secs(2));
        input.expire(now + Duration::from_secs(2) + VtInput::ESC_GRACE);
        assert_eq!(input.pop(), Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))));
        input.feed(b"j", now + Duration::from_secs(3));
        assert_eq!(input.pop(), Some(key('j')));
        assert_eq!(input.deadline(), None);
    }

    #[test]
    fn late_csi_suffixes_never_become_scope_keys_and_legacy_alt_prefixes_keep_parity() {
        let now = Instant::now();
        for reply in [b"\x1b[?1u".as_slice(), b"\x1b[6;20;10t", b"\x1b[?62;22c"] {
            for split in 4..reply.len() {
                let mut input = VtInput::default();
                input.feed(&reply[..split], now);
                input.expire(now + VtInput::ENVELOPE_GRACE);
                input.feed(&reply[split..], now + Duration::from_secs(2));
                assert!(!input.has_event(), "{reply:?} split {split}");
                input.feed(b"j", now + Duration::from_secs(2));
                assert_eq!(input.pop(), Some(key('j')));
            }
        }
        for letter in ['_', 'P', ']', '^', 'X'] {
            let mut input = VtInput::default();
            input.feed(format!("\x1b{letter}").as_bytes(), now);
            input.expire(now + VtInput::ESC_GRACE);
            assert_eq!(
                input.pop(),
                Some(Event::Key(KeyEvent::new(
                    KeyCode::Char(letter),
                    if letter.is_ascii_uppercase() {
                        KeyModifiers::ALT | KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::ALT
                    }
                )))
            );
        }
    }

    #[test]
    fn escape_then_another_sequence_preserves_the_key_and_owns_the_response() {
        let now = Instant::now();
        for delay in [1, 5, 34] {
            for bytes in [
                b"\x1b[A".as_slice(),
                b"\x1b[<0;10;5M",
                b"\x1b[O",
                b"\x1b[200~literal\x1b[201~",
                b"\x1b_Gi=7;OK\x1b\\",
                b"\x1b[6;20;10t",
            ] {
                let mut input = VtInput::default();
                input.feed(b"\x1b", now);
                input.feed(bytes, now + Duration::from_millis(delay));
                assert_eq!(
                    input.pop(),
                    Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
                );
                if bytes.starts_with(b"\x1b_G") || bytes.ends_with(b"t") {
                    assert!(!input.has_event());
                    assert_eq!(input.take_reply(), Some(bytes.to_vec()));
                } else {
                    let expected = terminput_crossterm::to_crossterm(
                        terminput::Event::parse_from(bytes).unwrap().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(input.pop(), Some(expected));
                    assert!(!input.has_event());
                }
            }
        }
    }

    #[test]
    fn complete_unknown_csi_does_not_stall_following_typing() {
        let now = Instant::now();
        for bytes in [b"\x1b[j".as_slice(), b"\x1b[?2004;1$y", b"\x1b[201~"] {
            let mut input = VtInput::default();
            input.feed(bytes, now);
            input.feed(b"k", now + Duration::from_millis(10));
            assert_eq!(input.pop(), Some(key('k')), "{bytes:?}");
            assert!(!input.stalled());
            assert_eq!(input.deadline(), None);
        }
    }

    #[test]
    fn fast_legacy_alt_control_prefixes_keep_typing_without_quarantine() {
        let now = Instant::now();
        for prefix in *b"_P]^X" {
            let mut input = VtInput::default();
            input.feed(&[0x1b, prefix], now);
            input.feed(b"j", now + Duration::from_millis(10));
            let expected = terminput_crossterm::to_crossterm(
                terminput::Event::parse_from(&[0x1b, prefix]).unwrap().unwrap(),
            )
            .unwrap();
            assert_eq!(input.pop(), Some(expected));
            assert_eq!(input.pop(), Some(key('j')));
            assert!(!input.stalled());
            assert_eq!(input.deadline(), None);
        }
    }

    #[test]
    fn editor_epoch_discards_only_a_recognized_response_tail() {
        let now = Instant::now();
        for (reply, first_split) in [
            (b"\x1b_Gi=123;OK\x1b\\".as_slice(), 3),
            (b"\x1b[?1u".as_slice(), 3),
            (b"\x1b[6;20;10t".as_slice(), 4),
        ] {
            for split in first_split..reply.len() {
                let mut before = VtInput::default();
                before.feed(b"e", now);
                before.feed(&reply[..split], now);
                assert_eq!(before.pop(), Some(key('e')));
                let mut after = before.discarded_handoff().unwrap();
                after.query_keyboard(now + Duration::from_secs(2));
                after.feed(&reply[split..], now + Duration::from_secs(2));
                after.feed(b"j", now + Duration::from_secs(2));
                assert_eq!(drain(&mut after), [key('j')]);
                assert_eq!(after.take_reply(), None);
                assert!(!after.take_keyboard_push());
                after.expire(now + Duration::from_secs(4));
                assert_eq!(after.deadline(), None);
            }
        }
        let mut before = VtInput::default();
        before.feed(b"\x1b", now);
        assert!(before.discarded_handoff().is_none());
        before.reset();
        before.feed(b"\x1b[200~old user paste", now);
        assert!(before.discarded_handoff().is_none());
    }

    #[test]
    fn keyboard_negotiation_needs_a_valid_reply_in_this_epoch() {
        let now = Instant::now();
        let mut input = VtInput::default();
        input.query_keyboard(now);
        input.feed(b"\x1b[?0", now);
        assert!(!input.take_keyboard_push());
        input.feed(b"u", now + Duration::from_millis(1));
        assert!(input.take_keyboard_push());
        assert!(!input.take_keyboard_push());
        assert!(input.keyboard_enabled());
        assert_eq!(input.deadline(), None);
        input.reset();
        input.query_keyboard(now);
        input.feed(b"\x1b[?0u", now + Duration::from_secs(1));
        assert!(!input.take_keyboard_push());
        assert!(!input.keyboard_enabled());
        assert_eq!(input.take_reply(), Some(b"\x1b[?0u".to_vec()));
        input.reset();
        input.query_keyboard(now);
        input.feed(b"\x1b[?bogusu", now);
        assert!(!input.take_keyboard_push());
        assert!(!input.has_event());
    }

    #[test]
    fn user_events_keep_the_existing_parser_parity_at_every_split() {
        let now = Instant::now();
        let rows = [
            "é",
            "日",
            "🙂",
            "\x1bx",
            "\x1bé",
            "\x1b🙂",
            "\x1b[A",
            "\x1bOD",
            "\x1b[[A",
            "\x1b[1;5D",
            "\x1b[120;3u",
            "\x1b[106;5:2u",
            "\x1b[106;5:3u",
            "\x1b[<0;10;5M",
            "\x1b[<64;10;5M",
            "\x1b[M !!",
            "\x1b[I",
            "\x1b[O",
        ];
        for text in rows {
            let expected = terminput_crossterm::to_crossterm(
                terminput::Event::parse_from(text.as_bytes()).unwrap().unwrap(),
            )
            .unwrap();
            for split in 0..=text.len() {
                let mut input = VtInput::default();
                input.feed(&text.as_bytes()[..split], now);
                input.feed(&text.as_bytes()[split..], now + Duration::from_millis(1));
                assert_eq!(
                    drain(&mut input).as_slice(),
                    std::slice::from_ref(&expected),
                    "{text:?} split {split}"
                );
                assert_eq!(input.take_reply(), None);
            }
        }
        let mut input = VtInput::default();
        input.feed(b"\xffj\xe2k\xf0\x80\x80\x80l", now);
        assert_eq!(drain(&mut input), [key('j'), key('k'), key('l')]);
        input.feed(b"\x1b[<64;10;5M", now);
        assert!(matches!(input.pop(), Some(Event::Mouse(e)) if e.kind == MouseEventKind::ScrollUp));
        input.feed(b"\x1b[I\x1b[O", now);
        assert_eq!(drain(&mut input), [Event::FocusGained, Event::FocusLost]);
    }

    #[test]
    fn reply_queue_is_bounded_and_reset_drops_every_kind_of_state() {
        let now = Instant::now();
        let mut input = VtInput::default();
        for _ in 0..100 {
            input.feed(b"\x1b_Gi=12;OK\x1b\\", now);
        }
        assert_eq!(input.replies.len(), VtInput::MAX_REPLIES);
        input.query_keyboard(now);
        input.feed(b"j\x1b[200~unfinished", now);
        input.resize((80, 24));
        input.reset();
        assert!(!input.has_event());
        assert_eq!(input.deadline(), None);
        assert_eq!(input.take_reply(), None);
        input.feed(b"k", now);
        assert_eq!(drain(&mut input), [key('k')]);
        input.feed(b"\x1b", now);
        input.reset();
        input.expire(now + Duration::from_secs(1));
        assert!(!input.has_event());
        assert_eq!(input.surrogate, None);
    }
}
