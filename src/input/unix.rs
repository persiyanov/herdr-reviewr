//! The Unix terminal's only reader; protocol envelopes never enter the user-event parser.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::Event;

use super::vt::VtInput;
use crate::wake::{Wake, Woke};

static READER: Mutex<Option<Reader>> = Mutex::new(None);
static RELEASED_SIZE: Mutex<Option<(u16, u16)>> = Mutex::new(None);
static RELEASED_PROTOCOL: Mutex<Option<VtInput>> = Mutex::new(None);

struct Reader {
    file: File,
    parser: VtInput,
    resized: Arc<AtomicBool>,
    resize_registration: signal_hook::SigId,
    size: Option<(u16, u16)>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.resize_registration);
    }
}

impl Reader {
    fn new(file: File) -> io::Result<Self> {
        let resized = Arc::new(AtomicBool::new(false));
        let resize_registration =
            signal_hook::flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&resized))?;
        let size = terminal_size(&file);
        Ok(Self { file, parser: VtInput::default(), resized, resize_registration, size })
    }

    fn fill(&mut self) -> io::Result<()> {
        let mut bytes = [0_u8; 8192];
        let count = match self.file.read(&mut bytes) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
            result => result?,
        };
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed"));
        }
        self.parser.feed(&bytes[..count], Instant::now());
        if self.parser.take_keyboard_push() {
            write_modes(b"\x1b[>1u");
            crate::logln!("keyboard enhancement supported");
        }
        Ok(())
    }

    fn observe_resize(&mut self) {
        let signalled = self.resized.swap(false, Ordering::SeqCst);
        self.note_size(terminal_size(&self.file), signalled);
    }

    fn note_size(&mut self, size: Option<(u16, u16)>, signalled: bool) {
        if let Some(size) = size {
            if signalled || self.size.is_some_and(|before| before != size) {
                self.parser.resize(size);
            }
            self.size = Some(size);
        }
    }

    fn timeout(&self, deadline: Option<Instant>) -> Option<Duration> {
        deadline
            .into_iter()
            .chain(self.parser.deadline())
            .min()
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    fn terminal_ready(&self, timeout: Option<Duration>) -> io::Result<bool> {
        let mut fds = [rustix::event::PollFd::from_borrowed_fd(
            self.file.as_fd(),
            rustix::event::PollFlags::IN,
        )];
        let timeout = timeout.and_then(|t| rustix::event::Timespec::try_from(t).ok());
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(_) => Ok(!fds[0].revents().is_empty()),
            Err(rustix::io::Errno::INTR) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now().checked_add(timeout);
        let mut reads = 0;
        loop {
            self.parser.expire(Instant::now());
            self.observe_resize();
            if self.parser.has_event() {
                return Ok(true);
            }
            // Always drain ready bytes, even when the caller requested a zero-time poll.
            if self.terminal_ready(Some(Duration::ZERO))? {
                if reads == 8 {
                    return Ok(false);
                }
                self.fill()?;
                reads += 1;
                if self.parser.has_reply() && !self.parser.has_event() {
                    return Ok(false);
                }
                continue;
            }
            if deadline.is_some_and(|at| at <= Instant::now()) {
                return Ok(false);
            }
            if self.terminal_ready(self.timeout(deadline))? {
                self.fill()?;
            }
        }
    }

    fn wait(&mut self, wake: &Wake, timeout: Option<Duration>) -> io::Result<Woke> {
        let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        self.parser.expire(Instant::now());
        self.observe_resize();
        if self.parser.has_event() {
            return Ok(Woke::Input);
        }
        if self.parser.has_reply() {
            return Ok(Woke::Woken);
        }
        let outcome =
            crate::wake::sys::wait(&[self.file.as_fd()], wake.sys(), self.timeout(deadline))?;
        if outcome == Woke::Input {
            self.fill()?;
        }
        self.parser.expire(Instant::now());
        self.observe_resize();
        Ok(if self.parser.has_event() {
            Woke::Input
        } else if outcome == Woke::Input {
            Woke::Woken
        } else {
            outcome
        })
    }
}

fn terminal_size(file: &File) -> Option<(u16, u16)> {
    rustix::termios::tcgetwinsize(file).ok().map(|size| (size.ws_col, size.ws_row))
}

fn with_reader<T>(f: impl FnOnce(&mut Reader) -> io::Result<T>) -> io::Result<T> {
    let mut guard = READER.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.is_none() {
        let stdin = io::stdin();
        let file = if rustix::termios::isatty(stdin.as_fd()) {
            stdin.as_fd().try_clone_to_owned()?.into()
        } else {
            File::open("/dev/tty")?
        };
        *guard = Some(Reader::new(file)?);
    }
    let reader = guard.as_mut().ok_or_else(|| io::Error::other("terminal reader unavailable"))?;
    f(reader)
}

fn write_modes(bytes: &[u8]) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(bytes).and_then(|()| stdout.flush());
}

/// Start a fresh input epoch and ask for keyboard support without consuming input elsewhere.
pub(crate) fn claim() {
    if let Err(error) = with_reader(|reader| {
        if reader.parser.keyboard_enabled() {
            write_modes(b"\x1b[<1u");
        }
        reader.parser = RELEASED_PROTOCOL
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        reader.resized.store(false, Ordering::SeqCst);
        reader.size = terminal_size(&reader.file);
        let previous = RELEASED_SIZE.lock().unwrap_or_else(PoisonError::into_inner).take();
        if previous.is_some_and(|previous| Some(previous) != reader.size)
            && let Some(size) = reader.size
        {
            reader.parser.resize(size);
        }
        reader.parser.query_keyboard(Instant::now());
        write_modes(b"\x1b[?u");
        Ok(())
    }) {
        crate::logln!("terminal input claim failed: {error}");
    }
}

/// Drop parser, reply, deadline and descriptor state before handing input to an editor.
pub(crate) fn release() {
    let mut reader = READER.lock().unwrap_or_else(PoisonError::into_inner);
    if reader.as_ref().is_some_and(|reader| reader.parser.keyboard_enabled()) {
        write_modes(b"\x1b[<1u");
    }
    *RELEASED_PROTOCOL.lock().unwrap_or_else(PoisonError::into_inner) =
        reader.as_ref().and_then(|r| r.parser.discarded_handoff());
    *RELEASED_SIZE.lock().unwrap_or_else(PoisonError::into_inner) =
        reader.as_ref().and_then(|r| r.size);
    *reader = None;
}

pub(crate) fn poll(timeout: Duration) -> io::Result<bool> {
    with_reader(|reader| reader.poll(timeout))
}

pub(crate) fn wait(wake: &Wake, timeout: Option<Duration>) -> io::Result<Woke> {
    with_reader(|reader| reader.wait(wake, timeout))
}

pub(crate) fn read() -> io::Result<Event> {
    with_reader(|reader| {
        loop {
            if let Some(event) = reader.parser.pop() {
                return Ok(event);
            }
            reader.poll(Duration::MAX)?;
        }
    })
}

/// Graphics, metrics and DA replies share this bounded queue; keyboard negotiation stays here.
pub(crate) fn take_graphics_reply() -> Option<Vec<u8>> {
    let mut reader = READER.lock().unwrap_or_else(PoisonError::into_inner);
    reader.as_mut()?.parser.take_reply()
}

/// An expired recognized control string needs its terminator or an explicit Esc recovery.
pub(crate) fn protocol_stalled() -> bool {
    READER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|reader| reader.parser.stalled())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    fn reader() -> (Reader, UnixStream) {
        let (read, write) = UnixStream::pair().unwrap();
        let fd: OwnedFd = read.into();
        (Reader::new(fd.into()).unwrap(), write)
    }

    fn key(ch: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
    }

    #[test]
    fn zero_time_poll_drains_large_paste_through_real_reads() {
        let (mut reader, mut writer) = reader();
        let body = format!("{}\x1b_Gi=12;OK\x1b\\{}", "x".repeat(9000), "y".repeat(9000));
        writer.write_all(format!("\x1b[200~{body}\x1b[201~jk").as_bytes()).unwrap();
        assert!(reader.poll(Duration::ZERO).unwrap());
        assert_eq!(reader.parser.pop(), Some(Event::Paste(body)));
        assert!(reader.poll(Duration::ZERO).unwrap());
        assert_eq!(reader.parser.pop(), Some(key('j')));
        assert!(reader.poll(Duration::ZERO).unwrap());
        assert_eq!(reader.parser.pop(), Some(key('k')));
        assert!(!reader.poll(Duration::ZERO).unwrap());
        assert_eq!(reader.parser.take_reply(), None);
    }

    #[test]
    fn wait_arms_its_own_escape_deadline_without_a_caller_timer() {
        let (mut reader, mut writer) = reader();
        let wake = Wake::new().unwrap();
        writer.write_all(b"\x1b").unwrap();
        assert!(!reader.poll(Duration::ZERO).unwrap());
        let began = Instant::now();
        assert_eq!(reader.wait(&wake, None).unwrap(), Woke::Input);
        assert!(began.elapsed() >= Duration::from_millis(15));
        assert_eq!(
            reader.parser.pop(),
            Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        );
        assert_eq!(reader.parser.deadline(), None);
    }

    #[test]
    fn a_split_reply_read_by_poll_wakes_the_session_without_emitting_keys() {
        let (mut reader, mut writer) = reader();
        let wake = Wake::new().unwrap();
        for bytes in [b"\x1b".as_slice(), b"_", b"Gi=12;OK\x1b", b"\\"] {
            writer.write_all(bytes).unwrap();
            assert!(!reader.poll(Duration::ZERO).unwrap());
        }
        assert_eq!(reader.wait(&wake, None).unwrap(), Woke::Woken);
        assert_eq!(reader.parser.take_reply(), Some(b"\x1b_Gi=12;OK\x1b\\".to_vec()));
        writer.write_all(b"j").unwrap();
        assert_eq!(reader.wait(&wake, None).unwrap(), Woke::Input);
        assert_eq!(reader.parser.pop(), Some(key('j')));
    }

    #[test]
    fn malformed_reply_keeps_its_late_tail_owned_before_keyboard_input_resumes() {
        let (mut reader, mut writer) = reader();
        let wake = Wake::new().unwrap();
        writer.write_all(b"\x1b_Gi=12;unterminated").unwrap();
        assert!(!reader.poll(Duration::ZERO).unwrap());
        assert_eq!(reader.wait(&wake, None).unwrap(), Woke::Deadline);
        assert_eq!(reader.parser.deadline(), None);
        assert!(!reader.parser.has_event());
        assert!(reader.parser.stalled());
        writer.write_all(b"late;OK\x1b\\j").unwrap();
        assert_eq!(reader.wait(&wake, None).unwrap(), Woke::Input);
        assert_eq!(reader.parser.pop(), Some(key('j')));
    }

    #[test]
    fn changed_sizes_and_same_size_signals_synthesize_resize_events() {
        let (mut reader, _writer) = reader();
        reader.note_size(Some((80, 24)), false);
        assert!(!reader.parser.has_event());
        reader.note_size(Some((90, 30)), false);
        assert_eq!(reader.parser.pop(), Some(Event::Resize(90, 30)));
        reader.note_size(Some((90, 30)), false);
        assert!(!reader.parser.has_event());
        reader.note_size(Some((90, 30)), true);
        assert_eq!(reader.parser.pop(), Some(Event::Resize(90, 30)));
        reader.note_size(None, true);
        assert!(!reader.parser.has_event());
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();
        assert!(reader.resized.swap(false, Ordering::SeqCst));
    }

    #[test]
    fn an_idle_reader_has_no_deadline_and_sleeps_until_the_existing_wake() {
        let (mut reader, _writer) = reader();
        let wake = Wake::new().unwrap();
        assert_eq!(reader.timeout(None), None);
        let waker = wake.waker();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.wake();
        });
        assert_eq!(reader.wait(&wake, None).unwrap(), Woke::Woken);
        worker.join().unwrap();
        assert_eq!(reader.timeout(None), None);
        assert!(!reader.parser.has_event());
    }
}
