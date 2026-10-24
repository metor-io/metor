//! Making a connection again once it is lost: [`Backoff`], the waits
//! between tries, and [`Reconnect`], a cyclic system's connection that
//! waits them out.

use core::time::Duration;

use super::IoError;
use super::cap::{Cap, Device, Handle, Interface};
use super::ops::{ConnectOp, TimeoutOp};
use super::queue::Pending;

/// The waits between tries: [`Backoff::INITIAL`] first, then, if it
/// doubles, twice the last up to [`Backoff::MAX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    delay: Duration,
    doubles: bool,
}

impl Backoff {
    pub const INITIAL: Duration = Duration::from_millis(500);
    pub const MAX: Duration = Duration::from_secs(10);

    pub const fn doubling() -> Self {
        Self {
            delay: Self::INITIAL,
            doubles: true,
        }
    }

    /// Every wait is [`Backoff::INITIAL`].
    pub const fn fixed() -> Self {
        Self {
            delay: Self::INITIAL,
            doubles: false,
        }
    }

    /// The next wait.
    pub fn next_wait(&mut self) -> Duration {
        let delay = self.delay;
        if self.doubles {
            self.delay = (delay * 2).min(Self::MAX);
        }
        delay
    }

    /// Starts the waits over, after a success.
    pub fn reset(&mut self) {
        self.delay = Self::INITIAL;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::doubling()
    }
}

/// A backoff's wait, kept on a capability's queue.
pub(crate) struct Pause {
    wait: Option<Pending<TimeoutOp>>,
    /// A wait is due but not yet on the queue.
    owed: bool,
    pub(crate) backoff: Backoff,
}

impl Pause {
    pub(crate) fn new(backoff: Backoff) -> Self {
        Self {
            wait: None,
            owed: false,
            backoff,
        }
    }

    /// Makes a wait due before the next try, unless one is running.
    pub(crate) fn start(&mut self) {
        self.owed |= self.wait.is_none();
    }

    /// Whether a wait is due or running. Puts a due wait on `cap`'s queue.
    pub(crate) fn waiting<K: Interface>(&mut self, cap: &Cap<K>) -> bool {
        if self.wait.as_mut().is_some_and(|w| w.poll().is_some()) {
            self.wait = None;
        }
        if self.owed {
            let mut wait = cap.timeout(self.backoff.delay);
            match wait.poll() {
                Some(Err(IoError::QueueFull)) => return true,
                Some(_) => {}
                None => self.wait = Some(wait),
            }
            self.owed = false;
            self.backoff.next_wait();
        }
        self.wait.is_some()
    }
}

/// What a [`Reconnect::poll`] saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconnected {
    /// A new connection is up.
    Up,
    /// A connect failed; the next waits out the backoff.
    Failed(IoError),
}

/// A connection to a `Cap<K>`'s resource, made again after a backoff
/// whenever it fails or is lost. Poll it once per cycle with the same
/// capability.
pub struct Reconnect<K: Device> {
    state: State<K>,
    pause: Pause,
}

enum State<K> {
    Down,
    Connecting(Pending<ConnectOp<K>>),
    Up(Handle<K>),
    /// Lost, until its close is on the queue.
    Closing(Handle<K>),
}

impl<K: Device> Reconnect<K> {
    /// Connects on the first poll, then backs off by doubling waits.
    pub fn new() -> Self {
        Self {
            state: State::Down,
            pause: Pause::new(Backoff::doubling()),
        }
    }

    /// Moves the connection to `cap`'s resource along.
    pub fn poll(&mut self, cap: &Cap<K>) -> Option<Reconnected> {
        match core::mem::replace(&mut self.state, State::Down) {
            State::Up(handle) => self.state = State::Up(handle),
            State::Closing(handle) => {
                if let Some(Err(IoError::QueueFull)) = handle.close().poll() {
                    self.state = State::Closing(handle);
                }
            }
            State::Connecting(mut pending) => match pending.poll() {
                None => self.state = State::Connecting(pending),
                Some(Ok(handle)) => {
                    self.state = State::Up(handle);
                    self.pause.backoff.reset();
                    return Some(Reconnected::Up);
                }
                Some(Err(IoError::QueueFull)) => {}
                Some(Err(e)) => {
                    self.pause.start();
                    return Some(Reconnected::Failed(e));
                }
            },
            State::Down if self.pause.waiting(cap) => {}
            State::Down => self.state = State::Connecting(cap.connect()),
        }
        None
    }

    /// The connection, while it is up.
    pub fn handle(&self) -> Option<&Handle<K>> {
        match &self.state {
            State::Up(handle) => Some(handle),
            _ => None,
        }
    }

    /// Closes the connection if `e` ends it, to make it again after the
    /// backoff. True if it did.
    pub fn check(&mut self, e: IoError) -> bool {
        let lost = e.is_lost() && matches!(self.state, State::Up(_));
        if lost {
            self.lose();
        }
        lost
    }

    /// Closes the connection, to make it again after the backoff.
    pub fn lose(&mut self) {
        if let State::Up(handle) = core::mem::replace(&mut self.state, State::Down) {
            self.state = State::Closing(handle);
            self.pause.start();
        }
    }
}

impl<K: Device> Default for Reconnect<K> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::test::{TestIo, cap};
    use crate::io::{Errno, Op, RawHandle, Uart};

    #[test]
    fn a_doubling_backoff_caps_at_ten_seconds_and_resets() {
        let mut backoff = Backoff::doubling();
        let waits: Vec<u128> = (0..7).map(|_| backoff.next_wait().as_millis()).collect();
        assert_eq!(waits, [500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000]);
        backoff.reset();
        assert_eq!(backoff.next_wait(), Backoff::INITIAL);
        let mut fixed = Backoff::fixed();
        assert_eq!(
            [fixed.next_wait(), fixed.next_wait()],
            [Backoff::INITIAL; 2]
        );
    }

    #[test]
    fn only_errors_that_end_a_connection_are_lost() {
        for lost in [Errno::EIO, Errno::ENODEV, Errno::ECONNRESET, Errno::EPIPE] {
            assert!(IoError::Os(lost).is_lost(), "{lost:?}");
        }
        assert!(IoError::Stale.is_lost());
        for kept in [
            IoError::Os(Errno::EAGAIN),
            IoError::Os(Errno::EINVAL),
            IoError::QueueFull,
            IoError::Canceled,
            IoError::Refused,
        ] {
            assert!(!kept.is_lost(), "{kept:?}");
        }
    }

    /// A connection, and the far end that answers its connects with `next`.
    struct Rig {
        cap: Cap<Uart>,
        io: TestIo,
        link: Reconnect<Uart>,
        next: Vec<Result<RawHandle, IoError>>,
        closed: Vec<RawHandle>,
    }

    impl Rig {
        fn new(next: Vec<Result<RawHandle, IoError>>) -> Self {
            let (cap, io) = cap::<Uart>();
            Self {
                cap,
                io,
                link: Reconnect::new(),
                next,
                closed: Vec::new(),
            }
        }

        /// One cycle: the far end answers, then the link polls.
        fn cycle(&mut self) -> Option<Reconnected> {
            let mut requests = self.io.turn().requests();
            while let Some(r) = requests.next() {
                match r.op() {
                    Op::Connect if !self.next.is_empty() => {
                        let next = self.next.remove(0);
                        r.done(next.map(|h| h.get() as usize));
                    }
                    Op::Close => {
                        self.closed.push(r.handle());
                        r.done(Ok(0));
                    }
                    _ => {}
                }
            }
            self.link.poll(&self.cap)
        }
    }

    const PORT: RawHandle = RawHandle::new(9, 1);

    #[test]
    fn it_connects_on_the_first_poll() {
        let mut r = Rig::new(vec![Ok(PORT)]);
        assert_eq!(r.cycle(), None);
        assert!(r.link.handle().is_none());
        assert_eq!(r.cycle(), Some(Reconnected::Up));
        assert_eq!(r.link.handle().map(Handle::raw), Some(PORT));
    }

    #[test]
    fn a_failed_connect_backs_off_before_the_next() {
        let missing = IoError::Os(Errno::ENOENT);
        let mut r = Rig::new(vec![Err(missing), Ok(PORT)]);
        r.cycle();
        assert_eq!(r.cycle(), Some(Reconnected::Failed(missing)));
        let up = (0..60).position(|_| r.cycle() == Some(Reconnected::Up));
        assert!(matches!(up, Some(49..=52)), "500 ms of 10 ms turns: {up:?}");
    }

    #[test]
    fn a_lost_connection_closes_and_connects_again() {
        let mut r = Rig::new(vec![Ok(PORT), Ok(RawHandle::new(10, 1))]);
        r.cycle();
        r.cycle();
        assert!(!r.link.check(IoError::Os(Errno::EAGAIN)), "not lost");
        assert!(r.link.handle().is_some());
        assert!(r.link.check(IoError::Os(Errno::EIO)));
        assert!(r.link.handle().is_none());
        assert!(!r.link.check(IoError::Os(Errno::EIO)), "already down");
        r.cycle();
        r.cycle();
        assert_eq!(r.closed, [PORT], "closed once");
        let up = (0..60).position(|_| r.cycle() == Some(Reconnected::Up));
        assert!(up.is_some());
        assert_eq!(
            r.link.handle().map(Handle::raw),
            Some(RawHandle::new(10, 1))
        );
    }

    #[test]
    fn polling_while_up_allocates_nothing() {
        let mut r = Rig::new(vec![Ok(PORT)]);
        r.cycle();
        r.cycle();
        let n = crate::io::tests::allocations(|| {
            for _ in 0..8 {
                assert_eq!(r.link.poll(&r.cap), None);
            }
        });
        assert_eq!(n, 0);
    }
}
