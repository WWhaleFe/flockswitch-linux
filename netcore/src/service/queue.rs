//! Bounded, ordered link output. Adjacent mouse motions share one queue slot.
use crate::{proto::Msg, InputEvent};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

struct State {
    messages: VecDeque<Msg>,
    receiving: bool,
}
struct Queue {
    state: Mutex<State>,
    ready: Condvar,
}
pub(super) struct Sender(Arc<Queue>);
pub(super) struct Receiver(Arc<Queue>);

pub(super) fn channel() -> (Sender, Receiver) {
    let queue = Arc::new(Queue {
        state: Mutex::new(State { messages: VecDeque::new(), receiving: true }),
        ready: Condvar::new(),
    });
    (Sender(queue.clone()), Receiver(queue))
}
impl Sender {
    pub fn try_send(&self, msg: Msg) -> Result<(), Msg> {
        let mut state = self.0.state.lock().unwrap();
        if !state.receiving { return Err(msg); }
        if let Msg::Input(InputEvent::Motion { dx, dy }) = &msg {
            if let Some(Msg::Input(InputEvent::Motion { dx: x, dy: y })) = state.messages.back_mut() {
                if (dx + *x).is_finite() && (dy + *y).is_finite() {
                    *x += dx;
                    *y += dy;
                    return Ok(());
                }
            }
        }
        if state.messages.len() == super::LINK_QUEUE { return Err(msg); }
        state.messages.push_back(msg);
        self.0.ready.notify_one();
        Ok(())
    }
}
impl Receiver {
    pub fn try_recv(&self) -> Result<Msg, ()> {
        self.0.state.lock().unwrap().messages.pop_front().ok_or(())
    }
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Msg, ()> {
        let state = self.0.state.lock().unwrap();
        let (mut state, _) = self.0.ready.wait_timeout_while(state, timeout,
            |s| s.messages.is_empty()).unwrap();
        state.messages.pop_front().ok_or(())
    }
    #[cfg(test)]
    pub fn try_iter(&self) -> impl Iterator<Item = Msg> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().receiving = false;
    }
}
