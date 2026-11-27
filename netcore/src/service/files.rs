//! One disk worker, one waiting receive and one send request per linked peer.
use super::{clip_sig, files_sig_cancellable};
use crate::transfer::{Incoming, Outgoing, Received};
use anyhow::Result;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};

pub(super) enum Work {
    Send {
        id: u32,
        paths: Vec<PathBuf>,
        limit: u64,
    },
    Receive {
        incoming: Incoming,
        dest: PathBuf,
    },
}
impl Work {
    fn direction(&self) -> usize {
        match self {
            Self::Send { .. } => 0,
            Self::Receive { .. } => 1,
        }
    }
    fn run(self, cancel: &AtomicBool) -> Result<Content> {
        match self {
            Self::Send { id, paths, limit } => {
                let out = Outgoing::files(id, &paths, limit)?;
                let sig = files_sig_cancellable(out.file_paths().expect("file transfer"), cancel)?;
                Ok(Content::Send(out, sig))
            }
            Self::Receive { incoming, dest } => {
                anyhow::ensure!(!cancel.load(Ordering::Acquire), "cancelled");
                let received = incoming.finish_cancellable(&dest, cancel)?;
                let mut receipt = Receipt { received: Some(received), sig: None, dest };
                receipt.sig = match receipt.received.as_ref().unwrap() {
                    Received::Files(paths) => Some(files_sig_cancellable(paths, cancel)?),
                    Received::Image(png) => Some(clip_sig(b"image", &[png])),
                    Received::Text(text) => Some(clip_sig(b"text", &[text.as_bytes()])),
                };
                anyhow::ensure!(!cancel.load(Ordering::Acquire), "cancelled");
                Ok(Content::Receive(receipt))
            }
        }
    }
}
pub(super) enum Content {
    Send(Outgoing, Vec<u8>),
    Receive(Receipt),
}
/// Owns published files until the service accepts this completion.
pub(super) struct Receipt {
    pub received: Option<Received>,
    pub sig: Option<Vec<u8>>,
    pub dest: PathBuf,
}
impl Drop for Receipt {
    fn drop(&mut self) {
        if let Some(Received::Files(paths)) = &self.received {
            for path in paths { let _ = std::fs::remove_file(path); }
        }
    }
}
pub(super) struct Job {
    pub peer: String,
    pub token: u64,
    pub revision: u64,
    pub work: Work,
}
struct Running {
    peer: String,
    token: u64,
    revision: u64,
    direction: usize,
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<Result<Content>>,
}
pub(super) struct Completion {
    pub peer: String,
    pub token: u64,
    pub revision: u64,
    pub direction: usize,
    pub result: Result<Content>,
}
#[derive(Default)]
pub(super) struct Jobs {
    running: Option<Running>,
    receive: Option<Job>,
    sends: VecDeque<Job>,
}
impl Jobs {
    #[cfg(test)]
    pub fn hold_worker(&mut self, peer: &str) -> std::sync::mpsc::Sender<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.running = Some(Running {
            peer: peer.into(),
            token: 1,
            revision: 0,
            direction: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            handle: thread::spawn(move || {
                rx.recv().unwrap();
                Err(anyhow::anyhow!("test worker released"))
            }),
        });
        tx
    }
    pub fn submit(&mut self, job: Job) {
        let direction = job.work.direction();
        if direction == 0 {
            self.sends.retain(|old| old.peer != job.peer);
            if let Some(r) = &self.running {
                if r.direction == 0 && r.peer == job.peer { r.cancel.store(true, Ordering::Release); }
            }
            self.sends.push_back(job);
        } else {
            self.cancel_direction(direction);
            self.receive = Some(job);
        }
    }
    pub fn cancel_direction(&mut self, direction: usize) -> bool {
        let mut cancelled = if direction == 1 { self.receive.take().is_some() }
            else { let queued = !self.sends.is_empty(); self.sends.clear(); queued };
        if let Some(r) = &self.running {
            if r.direction == direction {
                cancelled |= !r.cancel.swap(true, Ordering::AcqRel);
            }
        }
        cancelled
    }
    pub fn cancel_peer(&mut self, peer: &str) -> bool {
        let mut receiving = false;
        self.sends.retain(|job| job.peer != peer);
        if self.receive.as_ref().is_some_and(|job| job.peer == peer) {
            self.receive = None;
            receiving = true;
        }
        if let Some(r) = &self.running {
            if r.peer == peer {
                let was_cancelled = r.cancel.swap(true, Ordering::AcqRel);
                receiving |= r.direction == 1 && !was_cancelled;
            }
        }
        receiving
    }
    pub fn busy(&self) -> bool {
        self.running.is_some() || self.receive.is_some() || !self.sends.is_empty()
    }
    pub fn receiving(&self) -> bool {
        self.receive.is_some() || self.running.as_ref().is_some_and(|r|
            r.direction == 1 && !r.cancel.load(Ordering::Acquire))
    }
    pub fn poll(&mut self) -> Option<Completion> {
        if self
            .running
            .as_ref()
            .is_some_and(|r| r.handle.is_finished())
        {
            let r = self.running.take().unwrap();
            let result = r
                .handle
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("clipboard worker panicked")));
            if !r.cancel.load(Ordering::Acquire) {
                return Some(Completion {
                    peer: r.peer,
                    token: r.token,
                    revision: r.revision,
                    direction: r.direction,
                    result,
                });
            }
        }
        if self.running.is_none() {
            // Finish a received clipboard before preparing another outgoing one.
            if let Some(job) = self.receive.take().or_else(|| self.sends.pop_front()) {
                let direction = job.work.direction();
                let cancel = Arc::new(AtomicBool::new(false));
                let flag = cancel.clone();
                match thread::Builder::new()
                    .name("flock-clipboard".into())
                    .stack_size(256 * 1024)
                    .spawn(move || job.work.run(&flag))
                {
                    Ok(handle) => {
                        self.running = Some(Running {
                            peer: job.peer,
                            token: job.token,
                            revision: job.revision,
                            direction,
                            cancel,
                            handle,
                        })
                    }
                    Err(e) => {
                        return Some(Completion {
                            peer: job.peer,
                            token: job.token,
                            revision: job.revision,
                            direction,
                            result: Err(e.into()),
                        })
                    }
                }
            }
        }
        None
    }
    pub fn stop(&mut self) {
        self.receive = None;
        self.sends.clear();
        if let Some(r) = self.running.take() {
            r.cancel.store(true, Ordering::Release);
            let _ = r.handle.join();
        }
    }
}
impl Drop for Jobs {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn send_jobs_keep_other_peers_in_order_and_cancel_together() {
        let mut jobs = Jobs::default();
        let release = jobs.hold_worker("disk");
        for (peer, id) in [("b", 1), ("c", 2), ("c", 3)] {
            jobs.submit(Job { peer: peer.into(), token: 1, revision: id.into(),
                work: Work::Send { id, paths: vec![], limit: 100 } });
        }
        assert_eq!(jobs.sends.iter().map(|job| (job.peer.as_str(), job.revision))
            .collect::<Vec<_>>(), [("b", 1), ("c", 3)]);
        assert!(!jobs.running.as_ref().unwrap().cancel.load(Ordering::Acquire));
        assert!(jobs.cancel_direction(0));
        assert!(jobs.sends.is_empty());
        assert!(jobs.running.as_ref().unwrap().cancel.load(Ordering::Acquire));
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while jobs.busy() {
            assert!(jobs.poll().is_none());
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }

    #[test]
    fn only_the_latest_job_waits_and_cancelled_workers_are_reaped() {
        let mut jobs = Jobs::default();
        let release = jobs.hold_worker("new");
        for id in 1..100 {
            jobs.submit(Job {
                peer: "new".into(),
                token: 2,
                revision: id.into(),
                work: Work::Send {
                    id,
                    paths: vec![],
                    limit: 100,
                },
            });
        }
        assert!(jobs
            .running
            .as_ref()
            .unwrap()
            .cancel
            .load(Ordering::Acquire));
        assert_eq!(jobs.sends.len(), 1);
        assert_eq!(jobs.sends.front().unwrap().revision, 99);
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut completed = vec![];
        while jobs.busy() {
            if let Some(done) = jobs.poll() {
                completed.push(done.revision);
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(completed, [99]);
        assert!(jobs.running.is_none());
    }

    #[test]
    fn cancelling_a_completed_receive_removes_published_files() {
        let dir = std::env::temp_dir().join(format!("flock-completed-receive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let incoming = Incoming::start(43, crate::transfer::ClipKind::Files,
            vec![crate::transfer::FileMeta { name: "empty".into(), size: 0 }], 100, &dir).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let dest = dir.join("dest");
        let handle = thread::spawn(move || Work::Receive { incoming, dest }.run(&flag));
        while !handle.is_finished() { thread::yield_now(); }
        assert!(dir.join("dest/empty").exists());
        let mut jobs = Jobs::default();
        jobs.running = Some(Running { peer: "gone".into(), token: 1, revision: 1,
            direction: 1, cancel, handle });
        jobs.cancel_direction(1);
        jobs.poll();
        assert!(!dir.join("dest/empty").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelling_a_waiting_receive_cleans_its_directory() {
        let dir =
            std::env::temp_dir().join(format!("flock-waiting-receive-{}", std::process::id()));
        let incoming = Incoming::start(
            42,
            crate::transfer::ClipKind::Files,
            vec![crate::transfer::FileMeta {
                name: "empty".into(),
                size: 0,
            }],
            100,
            &dir,
        )
        .unwrap();
        let mut jobs = Jobs::default();
        jobs.submit(Job {
            peer: "gone".into(),
            token: 1,
            revision: 1,
            work: Work::Receive {
                incoming,
                dest: dir.join("dest"),
            },
        });
        jobs.cancel_peer("gone");
        assert!(!jobs.busy());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir(&dir).unwrap();
    }
}
