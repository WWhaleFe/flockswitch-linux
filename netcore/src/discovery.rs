use crate::{
    identity::Identity,
    proto::{Os, PROTO_VERSION},
};
use anyhow::Result;
use mdns_sd::{DaemonEvent, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{
    collections::HashMap,
    net::{SocketAddr, SocketAddrV6},
    time::{Duration, Instant},
};

pub const SERVICE_TYPE: &str = "_flockswitch._tcp.local.";
/// mdns-sd reports a service once when it is resolved, not on every refresh. So the
/// browse is started again this often to hear from every live peer again.
const REBROWSE: Duration = Duration::from_secs(60);
/// A peer not heard of in three browses is dropped (a "removed" event drops it sooner).
const FORGET: Duration = Duration::from_secs(200);
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub id: String,
    pub name: String,
    pub os: Os,
    pub addresses: Vec<SocketAddr>,
    pub port: u16,
    pub fingerprint: String,
    pub seen: Instant,
}
pub struct Discovery {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
    peers: HashMap<String, DiscoveredPeer>,
    monitor: mdns_sd::Receiver<DaemonEvent>,
    browsed: Instant,
}
impl Discovery {
    pub fn start(identity: &Identity, name: &str, os: Os, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let result = (|| {
            let os = serde_json::to_value(os)?.as_str().unwrap().to_owned();
            let props = HashMap::from([
                ("id".to_string(), identity.id.clone()),
                ("name".into(), name.into()),
                ("os".into(), os),
                ("proto".into(), PROTO_VERSION.to_string()),
                ("fp".into(), identity.fingerprint.clone()),
            ]);
            let service = ServiceInfo::new(
                SERVICE_TYPE,
                &identity.id,
                &format!("{}.local.", identity.id),
                "",
                port,
                props,
            )?
            .enable_addr_auto();
            daemon.register(service)?;
            let events = daemon.browse(SERVICE_TYPE)?;
            let monitor = daemon.monitor()?;
            Ok((events, monitor, HashMap::new()))
        })();
        match result {
            Ok((events, monitor, peers)) => Ok(Self {
                daemon,
                events,
                peers,
                monitor,
                browsed: Instant::now(),
            }),
            Err(e) => {
                let _ = daemon.shutdown();
                Err(e)
            }
        }
    }
    pub fn poll(&mut self) -> Vec<DiscoveredPeer> {
        if self.browsed.elapsed() >= REBROWSE {
            self.browsed = Instant::now();
            let _ = self.daemon.stop_browse(SERVICE_TYPE);
            match self.daemon.browse(SERVICE_TYPE) {
                Ok(events) => self.events = events,
                Err(e) => log::warn!("mDNS browse: {e}"),
            }
        }
        while let Ok(event) = self.monitor.try_recv() {
            if let DaemonEvent::Error(error) = event {
                log::warn!("mDNS: {error}");
            }
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let Some(id) = info.get_property_val_str("id") else {
                        continue;
                    };
                    let Some(fp) = info.get_property_val_str("fp") else {
                        continue;
                    };
                    if id.len() != 16
                        || fp.len() != 64
                        || !fp.starts_with(id)
                        || info.get_property_val_str("proto") != Some("1")
                    {
                        continue;
                    }
                    let os = match info.get_property_val_str("os") {
                        Some("mac") => Os::Mac,
                        Some("windows") => Os::Windows,
                        Some("linux") => Os::Linux,
                        Some("android") => Os::Android,
                        _ => continue,
                    };
                    let port = info.get_port();
                    let addresses = info
                        .get_addresses()
                        .iter()
                        .filter_map(|a| match a {
                            ScopedIp::V4(a) => Some(SocketAddr::new((*a.addr()).into(), port)),
                            ScopedIp::V6(a) => Some(SocketAddr::V6(SocketAddrV6::new(
                                *a.addr(),
                                port,
                                0,
                                a.scope_id().index,
                            ))),
                            _ => None,
                        })
                        .collect();
                    self.peers.insert(
                        id.into(),
                        DiscoveredPeer {
                            id: id.into(),
                            name: info.get_property_val_str("name").unwrap_or(id).into(),
                            os,
                            addresses,
                            port,
                            fingerprint: fp.into(),
                            seen: Instant::now(),
                        },
                    );
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    self.peers
                        .retain(|id, _| !fullname.starts_with(&format!("{id}.")));
                }
                _ => {}
            }
        }
        self.peers.retain(|_, p| p.seen.elapsed() < FORGET);
        self.peers.values().cloned().collect()
    }
}
impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}
