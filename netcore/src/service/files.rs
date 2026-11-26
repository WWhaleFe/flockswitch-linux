//! At most one disk worker and one waiting job per direction. No worker stays idle.
use super::{clip_sig, files_sig_cancellable};
use crate::transfer::{Incoming, Outgoing, Received};
use anyhow::Result;
use std::{
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
                let received = incoming.finish(&dest)?;
                let sig = match &received {
                    Received::Files(paths) => files_sig_cancellable(paths, cancel).ok(),
                    Received::Image(png) => Some(clip_sig(b"image", &[png])),
                    Received::Text(text) => Some(clip_sig(b"text", &[text.as_bytes()])),
                };
                Ok(Content::Receive(received, sig, dest))
            }
        }
    }
}
pub(super) enum Content {
    Send(Outgoing, Vec<u8>),
    Receive(Received, Option<Vec<u8>>, PathBuf),
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
    waiting: [Option<Job>; 2],
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
        self.cancel_direction(direction);
        self.waiting[direction] = Some(job);
    }
    pub fn cancel_direction(&mut self, direction: usize) -> bool {
        let mut cancelled = self.waiting[direction].take().is_some();
        if let Some(r) = &self.running {
            if r.direction == direction {
                cancelled |= !r.cancel.swap(true, Ordering::AcqRel);
            }
        }
        cancelled
    }
    pub fn cancel_peer(&mut self, peer: &str) -> bool {
        let mut receiving = false;
        for job in &mut self.waiting {
            if job.as_ref().is_some_and(|j| j.peer == peer) {
                receiving |= job.as_ref().unwrap().work.direction() == 1;
                *job = None;
            }
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
        self.running.is_some() || self.waiting.iter().any(Option::is_some)
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
            if let Some(job) = self.waiting[1].take().or_else(|| self.waiting[0].take()) {
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
        self.waiting = [None, None];
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
    fn only_the_latest_job_waits_and_cancelled_workers_are_reaped() {
        let mut jobs = Jobs::default();
        let release = jobs.hold_worker("old");
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
        assert_eq!(jobs.waiting.iter().flatten().count(), 1);
        assert_eq!(jobs.waiting[0].as_ref().unwrap().revision, 99);
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
