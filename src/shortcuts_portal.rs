//! Keyboard shortcuts on Wayland, through the system's shortcut service: the XDG desktop
//! portal's GlobalShortcuts. flockswitch asks for the same shortcuts the X11 version
//! grabs (one per channel while switching and the shortcuts are on, and the
//! find-the-cursor one), each with the key from the settings as its preferred trigger
//! ("CTRL+ALT+1"). The system shows its own window to approve them (GNOME: once per
//! shortcut; it then keeps the key the user chose, which may differ from the one asked
//! for) and says which keys it bound; pressing one sends "Activated", which does exactly
//! what the X11 shortcut does. A change of the shortcut settings closes the session and
//! binds again (a session binds only once). Nothing is saved by flockswitch: the system
//! keeps the keys under flockswitch's app id.
//!
//! The portal needs an app id. A host app tells it with org.freedesktop.host.portal.Registry
//! before any other portal call on its connection, so this thread has a D-Bus connection
//! of its own and registers "flockswitch" first (the portal accepts it only where
//! flockswitch.desktop is in the app menu: install.sh puts it there, the setup check does
//! for Homebrew). Without the portal, or when the request is turned down, the settings
//! window lists commands for the desktop's own custom shortcuts instead.
//!
//! Spoken to over zbus directly rather than through ashpd's GlobalShortcuts: ashpd
//! 0.13.10 wants a `trigger_description` with every bound shortcut (GNOME leaves it out
//! for a shortcut with no key, or with more than two), which would fail the whole answer,
//! its app id type refuses "flockswitch" (no dot), and its CreateSession asserts on the
//! session path. Here every answer is read as plain D-Bus values and whatever is missing
//! is simply not used.

use crate::config::Config;
use crate::edge_wayland::within;
use crate::locate_logic::{self as logic, PortalState, Wanted};
use crate::shared::{BoundShortcut, LocateSource, Shared};
use ashpd::zbus::{self, Connection, MatchRule, Message, MessageStream, Proxy};
use ashpd::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use futures_util::stream::{self, LocalBoxStream};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
/// The app id the system keeps the shortcuts under: the name of flockswitch.desktop.
const APP_ID: &str = "flockswitch";
/// How often the settings (and a request to open the system's settings) are looked at.
const TICK: Duration = Duration::from_millis(250);
/// A portal that is still starting (flockswitch started at login) answers a little
/// later: look again for a few minutes, as the edge probe does.
const PROBE_RETRIES: u32 = 10;
const PROBE_RETRY: Duration = Duration::from_secs(30);
/// A failed bind is tried again after this (times the failures so far), three times.
const RETRY: Duration = Duration::from_secs(20);

/// Starts the thread, in a Wayland session (the tray app).
pub fn spawn(shared: Arc<Shared>) {
    let started = std::thread::Builder::new().name("shortcuts".into()).spawn(move || async_io::block_on(run(shared)));
    if let Err(e) = started {
        log::warn!("shortcuts: cannot start the thread: {e}");
    }
}

fn set_state(shared: &Shared, state: PortalState, bound: Vec<BoundShortcut>) {
    shared.set_status(|st| {
        st.shortcuts_portal = state.code();
        st.shortcuts_bound = bound;
    });
}

/// What the settings ask the system for right now, as hotkey.rs registers on X11.
fn wanted_now(cfg: &Config) -> Vec<Wanted> {
    // The settings window records a shortcut: let go of every one, so it gets the keys.
    if crate::hotkey::paused() {
        return Vec::new();
    }
    let channels = (cfg.enabled && cfg.hotkeys.on).then(|| [cfg.hotkeys.key(1), cfg.hotkeys.key(2), cfg.hotkeys.key(3)]);
    let locate = (cfg.locate.on && cfg.locate.on_hotkey).then_some(cfg.locate.hotkey.as_str());
    logic::wanted(channels, locate)
}

/// A D-Bus error as the log shows it ("org.freedesktop.portal.Error.NotAllowed: …").
fn err_text(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, text, _) => format!("{}: {}", name.as_str(), text.as_deref().unwrap_or("")),
        other => other.to_string(),
    }
}

/// The portal (or the method) is not there at all.
fn is_missing(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::MethodError(name, _, _) => {
            let n = name.as_str();
            ["UnknownInterface", "UnknownMethod", "UnknownObject", "ServiceUnknown", "InvalidArgs", "NameHasNoOwner"].iter().any(|m| n.ends_with(m))
        }
        zbus::Error::InterfaceNotFound => true,
        _ => false,
    }
}

/// The portal refuses an app without an app id ("An app id is required").
fn is_no_app_id(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, text, _)
        if name.as_str().ends_with("NotAllowed") && text.as_deref().is_some_and(|t| t.to_lowercase().contains("app id")))
}

/// A value as itself, out of however many variants it is wrapped in.
fn plain<'a, 'b>(v: &'a Value<'b>) -> &'a Value<'b> {
    match v {
        Value::Value(inner) => plain(inner),
        other => other,
    }
}

fn text_of(v: &Value<'_>) -> Option<String> {
    match plain(v) {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        _ => None,
    }
}

/// The shortcuts of a BindShortcuts answer (`a(sa{sv})` in a variant): each id with how
/// the system says it is triggered, if it says.
fn shortcuts_in(v: &Value<'_>) -> Vec<(String, Option<String>)> {
    let Value::Array(items) = plain(v) else { return Vec::new() };
    items
        .inner()
        .iter()
        .filter_map(|item| {
            let Value::Structure(s) = plain(item) else { return None };
            let fields = s.fields();
            let id = text_of(fields.first()?)?;
            let trigger = fields.get(1).and_then(|d| match plain(d) {
                Value::Dict(d) => d.iter().find(|(k, _)| text_of(k).as_deref() == Some("trigger_description")).and_then(|(_, v)| text_of(v)),
                _ => None,
            });
            Some((id, trigger))
        })
        .collect()
}

/// The same from a ShortcutsChanged signal, whose shortcuts are not in a variant.
fn shortcuts_of(list: &[(String, HashMap<String, OwnedValue>)]) -> Vec<(String, Option<String>)> {
    list.iter().map(|(id, info)| (id.clone(), info.get("trigger_description").and_then(|v| text_of(v)))).collect()
}

fn bound_list(shortcuts: &[(String, Option<String>)]) -> Vec<BoundShortcut> {
    shortcuts
        .iter()
        .filter_map(|(id, trigger)| Some(BoundShortcut { slot: logic::slot_of(id)?, trigger: trigger.clone().unwrap_or_default() }))
        .collect()
}

/// A token for a request or session path: letters, digits and _ only.
fn token() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    format!("flockswitch_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

async fn proxy(conn: &Connection, path: String, interface: &'static str) -> zbus::Result<Proxy<'static>> {
    zbus::proxy::Builder::new(conn)
        .destination(DESTINATION)?
        .path(path)?
        .interface(interface)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

enum Ev {
    Activated { session: String, id: String },
    Changed { session: String, shortcuts: Vec<(String, Option<String>)> },
    Closed(String),
    Tick,
}

enum RequestError {
    /// The portal answered with this response code: 1 the request was cancelled, 2 it
    /// ended otherwise. GNOME answers 2 also when its approval window is closed.
    Answered(u32),
    NoAppId(String),
    Failed(String),
}

impl From<zbus::Error> for RequestError {
    fn from(e: zbus::Error) -> Self {
        if is_no_app_id(&e) {
            RequestError::NoAppId(err_text(&e))
        } else {
            RequestError::Failed(err_text(&e))
        }
    }
}

/// A bound session.
struct Live {
    session: String,
    bound: Vec<(String, Option<String>)>,
}

/// Why `serve` returned.
enum Served {
    Stop,
    /// Connect again now: a new connection registers the app id again (the app menu
    /// entry may be there now).
    Reconnect,
}

struct Portal {
    conn: Connection,
    gs: Proxy<'static>,
    version: u32,
    /// Our unique name as the portal's request and session paths spell it ("1_234").
    sender: String,
}

async fn run(shared: Arc<Shared>) {
    let mut tries = 0;
    loop {
        if shared.stopped() {
            return;
        }
        match Portal::connect().await {
            Ok(portal) => {
                tries = 0;
                match portal.serve(&shared).await {
                    Served::Stop => return,
                    Served::Reconnect => log::info!("shortcuts: connecting to the portal again"),
                }
            }
            Err(e) => {
                tries += 1;
                set_state(&shared, PortalState::Missing, Vec::new());
                if tries > PROBE_RETRIES {
                    log::info!("shortcuts: {e}; giving up: give the commands `flockswitch switch N` / `flockswitch locate` keys in the desktop's custom shortcuts");
                    return;
                }
                log::info!("shortcuts: {e}; looking again in {} s ({tries}/{PROBE_RETRIES})", PROBE_RETRY.as_secs());
                if !sleep(&shared, PROBE_RETRY).await {
                    return;
                }
            }
        }
    }
}

/// Waits, but not past a stop. False when flockswitch is stopping.
async fn sleep(shared: &Shared, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        if shared.stopped() {
            return false;
        }
        async_io::Timer::after(TICK).await;
    }
    !shared.stopped()
}

impl Portal {
    /// A connection of its own, the app id registered first, and the portal's version.
    async fn connect() -> Result<Portal, String> {
        let conn = Connection::session().await.map_err(|e| format!("no session bus: {e}"))?;
        // Must be the first portal call on this connection: the portal settles a
        // connection's app id with the first call it gets.
        match proxy(&conn, PATH.into(), "org.freedesktop.host.portal.Registry").await {
            Ok(registry) => {
                let options: HashMap<&str, Value<'_>> = HashMap::new();
                match registry.call_method("Register", &(APP_ID, options)).await {
                    Ok(_) => log::info!("shortcuts: app id '{APP_ID}' registered (org.freedesktop.host.portal.Registry)"),
                    Err(e) => log::info!(
                        "shortcuts: registering app id '{APP_ID}' failed ({}); the portal takes it from how flockswitch was started, which needs flockswitch.desktop in the app menu",
                        err_text(&e)
                    ),
                }
            }
            Err(e) => log::info!("shortcuts: no host app registry ({}); the portal takes the app id from how flockswitch was started", err_text(&e)),
        }
        let gs = proxy(&conn, PATH.into(), INTERFACE).await.map_err(|e| format!("GlobalShortcuts portal: {}", err_text(&e)))?;
        // A raw number: nothing here depends on how a library reads the portal.
        let version: u32 = match gs.get_property("version").await {
            Ok(v) => v,
            Err(e) if is_missing(&e) => return Err(format!("the desktop has no GlobalShortcuts portal ({})", err_text(&e))),
            Err(e) => return Err(format!("GlobalShortcuts portal: {}", err_text(&e))),
        };
        let sender = conn.unique_name().map(|n| n.as_str().trim_start_matches(':').replace('.', "_")).unwrap_or_default();
        Ok(Portal { conn, gs, version, sender })
    }

    /// The signals this thread follows, and a tick, as one stream that borrows nothing.
    async fn events(&self) -> Result<LocalBoxStream<'static, Ev>, String> {
        let err = |what: &str, e: zbus::Error| format!("listening for {what}: {}", err_text(&e));
        let activated = self.gs.receive_signal("Activated").await.map_err(|e| err("Activated", e))?;
        let changed = self.gs.receive_signal("ShortcutsChanged").await.map_err(|e| err("ShortcutsChanged", e))?;
        // A session the system ends (its settings, a portal restart) says so on its own path.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.portal.Session")
            .and_then(|b| b.member("Closed"))
            .map_err(|e| err("Closed", e))?
            .build();
        let closed = MessageStream::for_match_rule(rule, &self.conn, None).await.map_err(|e| err("Closed", e))?;
        let streams: Vec<LocalBoxStream<'static, Ev>> = vec![
            activated
                .filter_map(|m: Message| {
                    let body = m.body().deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)>().ok();
                    std::future::ready(body.map(|(session, id, _, _)| Ev::Activated { session: session.to_string(), id }))
                })
                .boxed_local(),
            changed
                .filter_map(|m: Message| {
                    let body = m.body().deserialize::<(OwnedObjectPath, Vec<(String, HashMap<String, OwnedValue>)>)>().ok();
                    std::future::ready(body.map(|(session, list)| Ev::Changed { session: session.to_string(), shortcuts: shortcuts_of(&list) }))
                })
                .boxed_local(),
            closed.filter_map(|m| std::future::ready(m.ok().and_then(|m| closed_session(&m)))).boxed_local(),
            async_io::Timer::interval(TICK).map(|_| Ev::Tick).boxed_local(),
        ];
        Ok(stream::select_all(streams).boxed_local())
    }

    async fn serve(self, shared: &Shared) -> Served {
        log::info!(
            "shortcuts: GlobalShortcuts portal v{} ({})",
            self.version,
            if self.version >= 2 { "the system's settings for them can be opened" } else { "version 1: the system's settings for them cannot be opened from here" }
        );
        let mut events = match self.events().await {
            Ok(e) => e,
            Err(e) => {
                log::warn!("shortcuts: {e}");
                set_state(shared, PortalState::Error, Vec::new());
                return if sleep(shared, PROBE_RETRY).await { Served::Reconnect } else { Served::Stop };
            }
        };
        let mut live: Option<Live> = None;
        // What the current state was made for: bound, refused or failed.
        let mut made_for: Option<Vec<Wanted>> = None;
        let mut refused: Vec<Wanted> = Vec::new();
        let mut failures: u32 = 0;
        let mut failed_at: Option<Instant> = None;
        let mut no_app_id = false;
        while let Some(ev) = events.next().await {
            if shared.stopped() {
                if let Some(l) = live.take() {
                    self.close(&l.session).await;
                }
                return Served::Stop;
            }
            match ev {
                Ev::Tick => {
                    let wanted = wanted_now(&shared.config());
                    if crate::shared::take_configure_shortcuts() {
                        self.configure(live.as_ref()).await;
                    }
                    let same = made_for.as_ref() == Some(&wanted);
                    // A failed bind is tried again after a while, three times at most.
                    let retry = same && failures < 3 && failed_at.is_some_and(|t| t.elapsed() >= RETRY * failures);
                    if same && !retry {
                        continue;
                    }
                    if no_app_id {
                        // A new connection registers the app id again.
                        return Served::Reconnect;
                    }
                    if retry {
                        log::info!("shortcuts: trying again ({failures} failed so far)");
                    } else {
                        failures = 0;
                    }
                    failed_at = None;
                    if let Some(l) = live.take() {
                        self.close(&l.session).await;
                    }
                    made_for = Some(wanted.clone());
                    if wanted.is_empty() {
                        log::info!(
                            "shortcuts: none to register ({})",
                            if crate::hotkey::paused() { "the settings window is recording a shortcut" } else { "the shortcuts are off" }
                        );
                        set_state(shared, PortalState::Off, Vec::new());
                        continue;
                    }
                    if logic::already_refused(&wanted, &refused) {
                        log::info!("shortcuts: not asking again: the system turned these down (change a shortcut, or restart flockswitch, to be asked again)");
                        set_state(shared, PortalState::Denied, Vec::new());
                        continue;
                    }
                    set_state(shared, PortalState::Asking, Vec::new());
                    match self.bind(shared, &wanted).await {
                        Ok(l) => {
                            set_state(shared, PortalState::Bound(self.version), bound_list(&l.bound));
                            live = Some(l);
                            refused.clear();
                            failures = 0;
                        }
                        // Not asked again by itself: the system's window would only come back.
                        Err(RequestError::Answered(code)) => {
                            log::info!(
                                "shortcuts: BindShortcuts answered {code}: not registered ({}); asked again when a shortcut changes or flockswitch restarts; custom shortcuts can run `flockswitch switch N` / `flockswitch locate` instead",
                                if code == 1 { "cancelled" } else { "turned down in the system's window, or an error there" }
                            );
                            refused = wanted;
                            set_state(shared, PortalState::Denied, Vec::new());
                        }
                        Err(RequestError::NoAppId(e)) => {
                            log::warn!(
                                "shortcuts: the portal wants an app id ({e}): flockswitch.desktop must be in the app menu (install.sh, or the setup check for Homebrew); asked again when the shortcut settings change or flockswitch restarts"
                            );
                            no_app_id = true;
                            set_state(shared, PortalState::NoAppId, Vec::new());
                        }
                        Err(RequestError::Failed(e)) => {
                            failures += 1;
                            failed_at = Some(Instant::now());
                            log::warn!("shortcuts: registering failed: {e}");
                            set_state(shared, PortalState::Error, Vec::new());
                        }
                    }
                }
                Ev::Activated { session, id } => {
                    if live.as_ref().is_none_or(|l| l.session != session) {
                        log::debug!("shortcuts: activated {id} of another session ({session})");
                        continue;
                    }
                    match logic::slot_of(&id) {
                        Some(logic::LOCATE_SLOT) => {
                            log::info!("shortcuts: activated '{id}': find the cursor");
                            shared.request_locate(LocateSource::Shortcut);
                        }
                        Some(n) => {
                            log::info!("shortcuts: activated '{id}': channel {n}");
                            shared.request_switch(n);
                        }
                        None => log::info!("shortcuts: activated an unknown shortcut '{id}'"),
                    }
                }
                Ev::Changed { session, shortcuts } => {
                    let Some(l) = live.as_mut().filter(|l| l.session == session) else { continue };
                    for (id, trigger) in &shortcuts {
                        log::info!("shortcuts: the system changed '{id}' → {}", describe(trigger.as_deref()));
                        match l.bound.iter_mut().find(|(b, _)| b == id) {
                            Some(entry) => entry.1 = trigger.clone(),
                            None => l.bound.push((id.clone(), trigger.clone())),
                        }
                    }
                    set_state(shared, PortalState::Bound(self.version), bound_list(&l.bound));
                }
                Ev::Closed(path) => {
                    if live.as_ref().is_some_and(|l| l.session == path) {
                        log::info!("shortcuts: the system ended session {path}; binding again shortly");
                        live = None;
                        failures += 1;
                        failed_at = Some(Instant::now());
                        set_state(shared, PortalState::Error, Vec::new());
                    }
                }
            }
        }
        log::info!("shortcuts: the portal's signals ended");
        if sleep(shared, PROBE_RETRY).await {
            Served::Reconnect
        } else {
            Served::Stop
        }
    }

    /// Calls a portal method that answers through a Request object, and waits for the
    /// answer (`limit` None: as long as it takes, e.g. while the system asks the user).
    async fn request<B>(&self, shared: &Shared, method: &'static str, token: &str, body: &B, limit: Option<Duration>) -> Result<HashMap<String, OwnedValue>, RequestError>
    where
        B: serde::Serialize + ashpd::zvariant::DynamicType,
    {
        let path = format!("/org/freedesktop/portal/desktop/request/{}/{token}", self.sender);
        let request = proxy(&self.conn, path.clone(), "org.freedesktop.portal.Request").await?;
        // Listening before the call, so the answer cannot come first.
        let mut responses = request.receive_signal("Response").await?;
        let reply = self.gs.call_method(method, body).await?;
        if let Ok(handle) = reply.body().deserialize::<OwnedObjectPath>() {
            if handle.as_str() != path {
                log::info!("shortcuts: {method} answers on {handle}, not {path}");
            }
        }
        let started = Instant::now();
        loop {
            if shared.stopped() {
                return Err(RequestError::Failed("flockswitch is stopping".into()));
            }
            if limit.is_some_and(|l| started.elapsed() >= l) {
                return Err(RequestError::Failed(format!("{method}: no answer in {} s", started.elapsed().as_secs())));
            }
            let Some(next) = within(Duration::from_millis(500), responses.next()).await else { continue };
            let Some(message) = next else { return Err(RequestError::Failed(format!("{method}: the answer never came"))) };
            let (code, results) = message
                .body()
                .deserialize::<(u32, HashMap<String, OwnedValue>)>()
                .map_err(|e| RequestError::Failed(format!("{method}: unreadable answer: {e}")))?;
            return if code == 0 { Ok(results) } else { Err(RequestError::Answered(code)) };
        }
    }

    /// Makes a session and binds `wanted` to it.
    async fn bind(&self, shared: &Shared, wanted: &[Wanted]) -> Result<Live, RequestError> {
        let (handle_token, session_token) = (token(), token());
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(handle_token.as_str()));
        options.insert("session_handle_token", Value::from(session_token.as_str()));
        // No question is asked here: any answer but 0 is a failure, tried again later.
        let results = match self.request(shared, "CreateSession", &handle_token, &(options,), Some(Duration::from_secs(30))).await {
            Err(RequestError::Answered(code)) => return Err(RequestError::Failed(format!("CreateSession: the portal answered {code}"))),
            other => other?,
        };
        let session = results
            .get("session_handle")
            .and_then(|v| text_of(v))
            .unwrap_or_else(|| format!("/org/freedesktop/portal/desktop/session/{}/{session_token}", self.sender));
        log::info!("shortcuts: session {session}");
        let t = crate::i18n::texts();
        let described: Vec<(String, String)> = wanted
            .iter()
            .map(|w| {
                let text = if w.slot == logic::LOCATE_SLOT { t.portal_desc_locate.to_string() } else { t.portal_desc_switch.replace("{n}", &w.slot.to_string()) };
                (w.id.clone(), text)
            })
            .collect();
        let shortcuts: Vec<(&str, HashMap<&str, Value<'_>>)> = wanted
            .iter()
            .zip(&described)
            .map(|(w, (id, text))| {
                let mut info: HashMap<&str, Value<'_>> = HashMap::new();
                info.insert("description", Value::from(text.as_str()));
                if let Some(trigger) = &w.trigger {
                    info.insert("preferred_trigger", Value::from(trigger.as_str()));
                }
                (id.as_str(), info)
            })
            .collect();
        let asked: Vec<String> = wanted.iter().map(|w| format!("'{}' {}", w.id, w.trigger.as_deref().unwrap_or("(no preferred key)"))).collect();
        log::info!("shortcuts: BindShortcuts {} (the system may ask)", asked.join(", "));
        let path = ObjectPath::try_from(session.as_str()).map_err(|e| RequestError::Failed(format!("session path '{session}': {e}")))?;
        let bind_token = token();
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(bind_token.as_str()));
        let answer = self.request(shared, "BindShortcuts", &bind_token, &(&path, shortcuts, "", options), None).await;
        let results = match answer {
            Ok(r) => r,
            Err(e) => {
                self.close(&session).await;
                return Err(e);
            }
        };
        let bound = results.get("shortcuts").map(|v| shortcuts_in(v)).unwrap_or_default();
        for w in wanted {
            match bound.iter().find(|(id, _)| *id == w.id) {
                Some((id, trigger)) => log::info!(
                    "shortcuts: bound '{id}' → {} (asked for {})",
                    describe(trigger.as_deref()),
                    w.trigger.as_deref().unwrap_or("no key")
                ),
                None => log::info!("shortcuts: '{}' was not bound by the system", w.id),
            }
        }
        Ok(Live { session, bound })
    }

    async fn close(&self, session: &str) {
        let closed = match proxy(&self.conn, session.to_string(), "org.freedesktop.portal.Session").await {
            Ok(p) => p.call_method("Close", &()).await.map(|_| ()),
            Err(e) => Err(e),
        };
        match closed {
            Ok(()) => log::info!("shortcuts: session {session} closed"),
            Err(e) => log::info!("shortcuts: closing session {session}: {}", err_text(&e)),
        }
    }

    /// Opens the system's own settings for these shortcuts (portal version 2).
    async fn configure(&self, live: Option<&Live>) {
        let Some(l) = live else {
            log::info!("shortcuts: no bound session to open the system's settings for");
            return;
        };
        if self.version < 2 {
            log::info!("shortcuts: the portal is version {}; ConfigureShortcuts needs 2", self.version);
            return;
        }
        let Ok(path) = ObjectPath::try_from(l.session.as_str()) else { return };
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        match self.gs.call_method("ConfigureShortcuts", &(&path, "", options)).await {
            Ok(_) => log::info!("shortcuts: the system's settings for them are open (ConfigureShortcuts)"),
            Err(e) => log::info!("shortcuts: ConfigureShortcuts: {}", err_text(&e)),
        }
    }
}

/// The session a Session.Closed signal is about.
fn closed_session(m: &Message) -> Option<Ev> {
    let header = m.header();
    let path = header.path()?.to_string();
    Some(Ev::Closed(path))
}

fn describe(trigger: Option<&str>) -> String {
    match trigger {
        Some(t) if !t.is_empty() => format!("'{t}'"),
        _ => "no key yet".into(),
    }
}
