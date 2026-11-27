//! Wake from sleep and screen unlock over D-Bus: the network thread looks for the other
//! computers again at once instead of waiting for its own timers.
//!
//! Listens to:
//! - system bus: `org.freedesktop.login1.Manager` `PrepareForSleep(false)` (resume)
//! - session bus: `org.freedesktop.ScreenSaver` and `org.gnome.ScreenSaver`
//!   `ActiveChanged(false)` (screen unlocked)

use crate::shared::Shared;
use ashpd::zbus::{self, Connection, MatchRule, MessageStream};
use futures_util::stream::{self, LocalBoxStream, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

static PENDING: AtomicBool = AtomicBool::new(false);

/// Records that the computer woke up or the screen was unlocked.
pub fn notify() {
    PENDING.store(true, Ordering::Release);
}

/// True once per wake-up or unlock.
pub fn take() -> bool {
    PENDING.swap(false, Ordering::AcqRel)
}

/// Both `PrepareForSleep(active)` and `ActiveChanged(active)` send `false` when the
/// computer comes back (waking up or unlocking), and `true` when going away.
fn should_reconnect(active: bool) -> bool {
    !active
}

/// Starts listening on a background thread until `shared.stopped()`.
pub fn spawn(shared: Arc<Shared>) {
    let _ = std::thread::Builder::new().name("wake".into()).spawn(move || {
        async_io::block_on(watch(shared));
    });
}

enum Ev {
    Tick,
    Signal(&'static str, bool),
}

async fn signal_stream(
    conn: &Connection,
    interface: &'static str,
    member: &'static str,
    kind: &'static str,
) -> Option<LocalBoxStream<'static, Ev>> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(interface)
        .ok()?
        .member(member)
        .ok()?
        .build();
    let stream = MessageStream::for_match_rule(rule, conn, None).await.ok()?;
    Some(
        stream
            .filter_map(move |msg| {
                let active = msg.ok().and_then(|m| m.body().deserialize::<bool>().ok());
                std::future::ready(active.map(|b| Ev::Signal(kind, b)))
            })
            .boxed_local(),
    )
}

async fn watch(shared: Arc<Shared>) {
    let mut streams: Vec<LocalBoxStream<'static, Ev>> = Vec::new();
    let sys = Connection::system().await.ok();
    if let Some(conn) = &sys {
        if let Some(s) = signal_stream(conn, "org.freedesktop.login1.Manager", "PrepareForSleep", "resume").await {
            streams.push(s);
        }
    }
    let sess = Connection::session().await.ok();
    if let Some(conn) = &sess {
        if let Some(s) = signal_stream(conn, "org.freedesktop.ScreenSaver", "ActiveChanged", "unlock").await {
            streams.push(s);
        }
        if let Some(s) = signal_stream(conn, "org.gnome.ScreenSaver", "ActiveChanged", "gnome-unlock").await {
            streams.push(s);
        }
    }
    if streams.is_empty() {
        log::info!("wake: neither login1 nor ScreenSaver D-Bus signals are available");
        return;
    }
    log::info!("wake: watching {} D-Bus wake/unlock signal(s)", streams.len());
    streams.push(async_io::Timer::interval(Duration::from_secs(1)).map(|_| Ev::Tick).boxed_local());
    let mut events = stream::select_all(streams);
    while let Some(ev) = events.next().await {
        if shared.stopped() {
            return;
        }
        if let Ev::Signal(kind, active) = ev {
            if should_reconnect(active) {
                log::info!("wake: {kind} signal received");
                notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_taken_once() {
        take();
        assert!(!take());
        notify();
        notify();
        assert!(take());
        assert!(!take());
    }

    #[test]
    fn only_false_signal_triggers_reconnect() {
        assert!(should_reconnect(false));
        assert!(!should_reconnect(true));
    }
}
