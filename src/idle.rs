//! Generation-based wakeups: a notification before waiting must not be lost.
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Signal {
    generation: Mutex<u64>,
    changed: Condvar,
    #[cfg(target_os = "linux")]
    readiness: Mutex<std::sync::Weak<std::os::fd::OwnedFd>>,
}

impl Signal {
    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap()
    }

    pub fn notify(&self) {
        let mut generation = self.generation.lock().unwrap();
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
        #[cfg(target_os = "linux")]
        if let Some(fd) = self.readiness.lock().unwrap().upgrade() {
            loop {
                match rustix::io::write(&*fd, &1u64.to_ne_bytes()) {
                    Err(rustix::io::Errno::INTR) => continue,
                    _ => break,
                }
            }
        }
    }

    pub fn wait(&self, seen: u64, timeout: Duration) {
        let generation = self.generation.lock().unwrap();
        drop(self.changed.wait_timeout_while(generation, timeout, |current| *current == seen).unwrap());
    }

    /// One fd listener, for a Wayland dispatch that must also wake on app changes.
    /// A weak reference lets the fd close when that watcher stops.
    #[cfg(target_os = "linux")]
    pub fn readiness(&self) -> std::io::Result<std::sync::Arc<std::os::fd::OwnedFd>> {
        let mut listener = self.readiness.lock().unwrap();
        if let Some(fd) = listener.upgrade() { return Ok(fd); }
        let fd = std::sync::Arc::new(rustix::event::eventfd(0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK)?);
        *listener = std::sync::Arc::downgrade(&fd);
        Ok(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};

    #[test]
    fn notification_before_wait_is_not_lost() {
        let signal = Arc::new(Signal::default());
        let seen = signal.generation();
        signal.notify();
        let (done, received) = mpsc::channel();
        std::thread::spawn(move || {
            signal.wait(seen, Duration::from_secs(30));
            done.send(()).unwrap();
        });
        received.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn notification_wakes_all_waiters() {
        let signal = Arc::new(Signal::default());
        let seen = signal.generation();
        let (done, received) = mpsc::channel();
        let workers: Vec<_> = (0..2).map(|_| {
            let signal = signal.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                signal.wait(seen, Duration::from_secs(30));
                done.send(()).unwrap();
            })
        }).collect();
        signal.notify();
        for _ in 0..2 { received.recv_timeout(Duration::from_secs(2)).unwrap(); }
        for worker in workers { worker.join().unwrap(); }
    }

    #[test]
    fn idle_wait_reaches_its_timeout() {
        let signal = Signal::default();
        let start = std::time::Instant::now();
        signal.wait(signal.generation(), Duration::from_millis(25));
        assert!(start.elapsed() >= Duration::from_millis(25));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fd_notifications_coalesce_and_the_listener_is_released() {
        let signal = Signal::default();
        let fd = signal.readiness().unwrap();
        let weak = Arc::downgrade(&fd);
        signal.notify();
        signal.notify();
        let mut count = [0u8; 8];
        assert_eq!(rustix::io::read(&*fd, &mut count).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(count), 2);
        drop(fd);
        assert!(weak.upgrade().is_none());
        signal.notify();
        let fresh = signal.readiness().unwrap();
        assert_eq!(rustix::io::read(&*fresh, &mut count), Err(rustix::io::Errno::AGAIN));
    }
}
