//! The frame loop's wake: a worker's result, a window resize, or a deadline wakes it. A socket
//! pair on unix, an auto-reset event on Windows; nothing in it wakes because time passed.

use std::io;
use std::sync::{Arc, mpsc};

/// What ended a wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    /// The terminal has input to read.
    Input,
    /// A worker finished, or the window resized.
    Woken,
    /// The deadline passed with nothing else to do.
    Deadline,
}

/// The waiting side. Each wait drains its wakes, so the next one sleeps again.
#[derive(Debug)]
pub struct Wake {
    sys: sys::Wake,
    waker: Waker,
}

impl Wake {
    pub fn new() -> io::Result<Self> {
        let (sys, waker) = sys::Wake::new()?;
        Ok(Self { sys, waker: Waker(Some(Arc::new(waker))) })
    }

    /// A handle any thread wakes this one through.
    pub fn waker(&self) -> Waker {
        self.waker.clone()
    }

    /// Sleep until a wake or `timeout` (`None` waits for ever): `Woken` or `Deadline`. The frame
    /// loop waits through `input::wait`; this is the waker's test seam.
    #[cfg(test)]
    pub fn wait(&self, timeout: Option<std::time::Duration>) -> io::Result<Woke> {
        self.sys.wait(timeout)
    }

    /// The platform half the input layer waits on beside the terminal.
    pub(crate) fn sys(&self) -> &sys::Wake {
        &self.sys
    }
}

/// Wakes a [`Wake`]. Cheap to clone and safe from any thread; a wake already pending is enough.
#[derive(Clone, Debug)]
pub struct Waker(Option<Arc<sys::Waker>>);

impl Waker {
    /// A waker with nothing behind it, for tests that drive workers directly.
    pub fn detached() -> Self {
        Self(None)
    }

    pub fn wake(&self) {
        if let Some(sys) = &self.0 {
            sys.wake();
        }
    }
}

/// A channel sender that wakes the receiving thread after every send, and when it drops, so a
/// worker that ends or dies is seen at once.
pub struct Sender<T> {
    tx: mpsc::Sender<T>,
    waker: Waker,
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wake::Sender")
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), waker: self.waker.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.waker.wake();
    }
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        self.tx.send(value)?;
        self.waker.wake();
        Ok(())
    }
}

/// A channel whose sends wake the thread behind `waker`.
pub fn channel<T>(waker: &Waker) -> (Sender<T>, mpsc::Receiver<T>) {
    let (tx, rx) = mpsc::channel();
    (Sender { tx, waker: waker.clone() }, rx)
}

#[cfg(unix)]
pub(crate) mod sys {
    use std::io::{self, Read, Write};
    use std::os::fd::{AsFd, BorrowedFd};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::time::Duration;

    use rustix::event::{PollFd, PollFlags, Timespec};

    use super::Woke;

    /// The read end of a socket pair every wake writes a byte to.
    #[derive(Debug)]
    pub(crate) struct Wake {
        pipe: UnixStream,
        write: Arc<UnixStream>,
    }

    #[derive(Debug)]
    pub(crate) struct Waker(Arc<UnixStream>);

    /// SIGWINCH writing to one wake, until dropped: a switched pane's next loop registers its own.
    #[must_use]
    pub(crate) struct ResizeWatch(signal_hook::SigId);

    impl Drop for ResizeWatch {
        fn drop(&mut self) {
            signal_hook::low_level::unregister(self.0);
        }
    }

    impl Wake {
        pub(super) fn new() -> io::Result<(Self, Waker)> {
            let (pipe, write) = UnixStream::pair()?;
            pipe.set_nonblocking(true)?;
            write.set_nonblocking(true)?;
            let write = Arc::new(write);
            Ok((Self { pipe, write: Arc::clone(&write) }, Waker(write)))
        }

        #[cfg(test)]
        pub(super) fn wait(&self, timeout: Option<Duration>) -> io::Result<Woke> {
            wait(&[], self, timeout)
        }

        /// Write a byte on every SIGWINCH; crossterm still reads the resize itself.
        pub(crate) fn watch_resizes(&self) -> io::Result<ResizeWatch> {
            let id = signal_hook::low_level::pipe::register(
                signal_hook::consts::SIGWINCH,
                self.write.try_clone()?,
            )?;
            Ok(ResizeWatch(id))
        }

        pub(crate) fn fd(&self) -> BorrowedFd<'_> {
            self.pipe.as_fd()
        }

        fn drain(&self) {
            let mut buf = [0_u8; 64];
            while matches!((&self.pipe).read(&mut buf), Ok(n) if n > 0) {}
        }
    }

    impl Waker {
        pub(super) fn wake(&self) {
            // A full pipe drops the byte: one unread byte already guarantees the wake.
            let _ = (&*self.0).write(&[1]);
        }
    }

    /// Sleep until one of `inputs` is readable (`Input`), `wake` is (`Woken`), or `timeout`.
    pub(crate) fn wait(
        inputs: &[BorrowedFd<'_>],
        wake: &Wake,
        timeout: Option<Duration>,
    ) -> io::Result<Woke> {
        // A deadline too far out to represent is no deadline.
        let timeout = timeout.and_then(|t| Timespec::try_from(t).ok());
        let mut fds: Vec<PollFd<'_>> =
            inputs.iter().map(|fd| PollFd::from_borrowed_fd(*fd, PollFlags::IN)).collect();
        fds.push(PollFd::from_borrowed_fd(wake.fd(), PollFlags::IN));
        loop {
            match rustix::event::poll(&mut fds, timeout.as_ref()) {
                Ok(_) => break,
                // A signal (the resize) interrupted the wait; its handler wrote to the pipe.
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let (wake_fd, input_fds) = fds.split_last().expect("the wake is always polled");
        let woken = !wake_fd.revents().is_empty();
        if woken {
            wake.drain();
        }
        Ok(if input_fds.iter().any(|fd| !fd.revents().is_empty()) {
            Woke::Input
        } else if woken {
            Woke::Woken
        } else {
            Woke::Deadline
        })
    }
}

#[cfg(windows)]
pub(crate) mod sys {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::sync::Arc;
    use std::time::Duration;

    #[cfg(test)]
    use super::Woke;

    /// An auto-reset event: a wait that sees it set clears it, so the next wait sleeps again.
    #[derive(Debug)]
    pub(crate) struct Wake(Arc<OwnedHandle>);

    #[derive(Debug)]
    pub(crate) struct Waker(Arc<OwnedHandle>);

    impl Wake {
        #[allow(unsafe_code)]
        pub(super) fn new() -> io::Result<(Self, Waker)> {
            use windows_sys::Win32::System::Threading::CreateEventW;
            // SAFETY: null attributes and name ask for an unnamed event; the result is checked.
            let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `event` is a fresh handle that nothing else owns.
            let event = Arc::new(unsafe { OwnedHandle::from_raw_handle(event.cast()) });
            Ok((Self(Arc::clone(&event)), Waker(event)))
        }

        #[cfg(test)]
        pub(super) fn wait(&self, timeout: Option<Duration>) -> io::Result<Woke> {
            Ok(match wait_any(&[self.handle()], timeout)? {
                Some(_) => Woke::Woken,
                None => Woke::Deadline,
            })
        }

        pub(crate) fn handle(&self) -> RawHandle {
            self.0.as_raw_handle()
        }
    }

    impl Waker {
        #[allow(unsafe_code)]
        pub(super) fn wake(&self) {
            use windows_sys::Win32::System::Threading::SetEvent;
            // SAFETY: the event handle stays open for as long as this `Arc` holds it.
            unsafe { SetEvent(self.0.as_raw_handle().cast()) };
        }
    }

    /// Sleep until one of `handles` is signaled, or `timeout`: the lowest signaled index, or `None`.
    #[allow(unsafe_code)]
    pub(crate) fn wait_any(
        handles: &[RawHandle],
        timeout: Option<Duration>,
    ) -> io::Result<Option<usize>> {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

        let raw: Vec<_> = handles.iter().map(|h| h.cast()).collect();
        // Rounded up, so a wait just short of its deadline sleeps instead of spinning.
        let millis = timeout.map_or(INFINITE, |timeout| {
            u32::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(INFINITE - 1)
        });
        let count = u32::try_from(raw.len()).expect("a handful of handles");
        // SAFETY: every handle is open for the call, and the array outlives it.
        let signaled = unsafe { WaitForMultipleObjects(count, raw.as_ptr(), 0, millis) };
        let index = signaled.wrapping_sub(WAIT_OBJECT_0) as usize;
        if index < handles.len() {
            Ok(Some(index))
        } else if signaled == WAIT_TIMEOUT {
            Ok(None)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Wake, Woke, channel};
    use std::time::{Duration, Instant};

    #[test]
    fn a_quiet_wait_sleeps_until_its_deadline() {
        let wake = Wake::new().unwrap();
        let started = Instant::now();
        assert_eq!(wake.wait(Some(Duration::from_millis(30))).unwrap(), Woke::Deadline);
        // A Windows wait may end up to one 15.6ms timer tick early; the loop then waits again.
        assert!(started.elapsed() >= Duration::from_millis(14), "it slept until the deadline");
    }

    #[test]
    fn a_send_from_another_thread_wakes_the_wait_once() {
        let wake = Wake::new().unwrap();
        let (tx, rx) = channel::<u32>(&wake.waker());
        let worker = std::thread::spawn(move || tx.send(7).unwrap());
        worker.join().unwrap();
        assert_eq!(wake.wait(Some(Duration::from_secs(5))).unwrap(), Woke::Woken);
        assert_eq!(rx.try_recv().unwrap(), 7, "the value is there when the wait returns");
        assert_eq!(
            wake.wait(Some(Duration::from_millis(10))).unwrap(),
            Woke::Deadline,
            "the wake was taken, so the next wait sleeps again"
        );
    }

    #[test]
    fn many_wakes_before_a_wait_collapse_into_one() {
        let wake = Wake::new().unwrap();
        let waker = wake.waker();
        for _ in 0..10_000 {
            waker.wake();
        }
        assert_eq!(wake.wait(Some(Duration::from_secs(1))).unwrap(), Woke::Woken);
        assert_eq!(wake.wait(Some(Duration::from_millis(10))).unwrap(), Woke::Deadline);
    }

    #[test]
    fn a_worker_that_ends_wakes_the_wait() {
        let wake = Wake::new().unwrap();
        let (tx, _rx) = channel::<u32>(&wake.waker());
        std::thread::spawn(move || drop(tx)).join().unwrap();
        assert_eq!(wake.wait(Some(Duration::from_secs(5))).unwrap(), Woke::Woken);
    }

    #[cfg(unix)]
    #[test]
    fn a_window_resize_wakes_the_wait() {
        let wake = Wake::new().unwrap();
        let watch = wake.sys().watch_resizes().unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();
        assert_eq!(wake.wait(Some(Duration::from_secs(5))).unwrap(), Woke::Woken);

        // A switched pane's old loop drops its watch: resizes reach only the new loop's wake.
        let next = Wake::new().unwrap();
        drop(watch);
        let _next_watch = next.sys().watch_resizes().unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();
        assert_eq!(next.wait(Some(Duration::from_secs(5))).unwrap(), Woke::Woken);
        assert_eq!(wake.wait(Some(Duration::from_millis(50))).unwrap(), Woke::Deadline);
    }
}
