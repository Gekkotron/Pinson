//! mDNS discovery of Chromecasts and snapservers on the local network.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::sync::watch;

const CAST_SERVICE: &str = "_googlecast._tcp.local.";
const SNAP_SERVICE: &str = "_snapcast._tcp.local.";

/// A Chromecast (or other Cast receiver) found on the network.
#[derive(Clone, Debug, PartialEq)]
pub struct CastDevice {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub addr: SocketAddr,
}

/// A snapserver found on the network.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapServer {
    pub name: String,
    pub host: IpAddr,
}

/// Everything discovered so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    /// Sorted by name, then id.
    pub casts: Vec<CastDevice>,
    /// Sorted by host.
    pub snapservers: Vec<SnapServer>,
}

/// Removes `-`; `Some(lowercase)` when exactly 32 hex digits remain.
pub fn normalize_cast_id(s: &str) -> Option<String> {
    let id: String = s.chars().filter(|c| *c != '-').collect();
    (id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| id.to_lowercase())
}

/// Builds a device from a resolved mDNS record; needs an id and an IPv4 address.
pub(crate) fn cast_from(
    fullname: &str,
    id: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    port: u16,
    v4: &[Ipv4Addr],
) -> Option<CastDevice> {
    let id = id.map(|i| normalize_cast_id(i).unwrap_or_else(|| i.to_lowercase()))?;
    let ip = *v4.iter().min()?;
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fullname.split("._").next().unwrap_or(fullname).to_string());
    Some(CastDevice {
        id,
        name,
        model: model.filter(|m| !m.is_empty()).map(str::to_string),
        addr: SocketAddr::new(IpAddr::V4(ip), port),
    })
}

/// Live view of what mDNS has found.
#[derive(Clone)]
pub struct Discovery {
    rx: watch::Receiver<Found>,
}

#[derive(Default)]
struct State {
    casts: HashMap<String, CastDevice>,
    snaps: HashMap<String, SnapServer>,
}

impl State {
    fn found(&self) -> Found {
        let mut casts: Vec<_> = self.casts.values().cloned().collect();
        casts.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        let mut snapservers: Vec<_> = self.snaps.values().cloned().collect();
        snapservers.sort_by_key(|s| s.host);
        Found { casts, snapservers }
    }
}

impl Discovery {
    /// Starts browsing; must run inside the tokio runtime. A failing daemon acts like `none()`.
    pub fn spawn() -> Self {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("mDNS discovery is off: {e}");
                return Self::none();
            }
        };
        let (tx, rx) = watch::channel(Found::default());
        let tx = Arc::new(tx);
        let state = Arc::new(Mutex::new(State::default()));
        for service in [CAST_SERVICE, SNAP_SERVICE] {
            let events = match daemon.browse(service) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("mDNS browse {service} failed: {e}");
                    continue;
                }
            };
            let (daemon, tx, state) = (daemon.clone(), tx.clone(), state.clone());
            tokio::spawn(async move {
                let _daemon = daemon; // keeps the daemon alive
                while let Ok(event) = events.recv_async().await {
                    let mut st = state.lock().unwrap();
                    match event {
                        ServiceEvent::ServiceResolved(info) => {
                            let v4: Vec<Ipv4Addr> = info.get_addresses_v4().into_iter().collect();
                            if service == CAST_SERVICE {
                                let dev = cast_from(
                                    &info.fullname,
                                    info.get_property_val_str("id"),
                                    info.get_property_val_str("fn"),
                                    info.get_property_val_str("md"),
                                    info.port,
                                    &v4,
                                );
                                match dev {
                                    Some(d) => {
                                        st.casts.insert(info.fullname.clone(), d);
                                    }
                                    None => {
                                        st.casts.remove(&info.fullname);
                                    }
                                }
                            } else if let Some(ip) = v4.iter().min() {
                                let name = info
                                    .fullname
                                    .split("._")
                                    .next()
                                    .unwrap_or(&info.fullname)
                                    .to_string();
                                st.snaps.insert(
                                    info.fullname.clone(),
                                    SnapServer {
                                        name,
                                        host: IpAddr::V4(*ip),
                                    },
                                );
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            st.casts.remove(&fullname);
                            st.snaps.remove(&fullname);
                        }
                        _ => continue,
                    }
                    let found = st.found();
                    drop(st);
                    tx.send_if_modified(|cur| {
                        if *cur == found {
                            false
                        } else {
                            *cur = found;
                            true
                        }
                    });
                }
            });
        }
        Discovery { rx }
    }

    /// Finds nothing, ever.
    pub fn none() -> Self {
        Self::manual().0
    }

    /// For tests: the caller sets what is "found".
    pub fn manual() -> (Self, watch::Sender<Found>) {
        let (tx, rx) = watch::channel(Found::default());
        (Discovery { rx }, tx)
    }

    pub fn found(&self) -> Found {
        self.rx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Found> {
        self.rx.clone()
    }

    /// A device by Cast id (dashes and case ignored) or by name (case-insensitive).
    pub fn cast(&self, key: &str) -> Option<CastDevice> {
        let found = self.rx.borrow();
        if let Some(id) = normalize_cast_id(key)
            && let Some(d) = found.casts.iter().find(|d| d.id == id)
        {
            return Some(d.clone());
        }
        found
            .casts
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(key.trim()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn cast_from_reads_txt() {
        let d = cast_from(
            "Chromecast-x._googlecast._tcp.local.",
            Some("0123456789abcdef0123456789ABCDEF"),
            Some("Living Room"),
            Some("Google Nest Mini"),
            8009,
            &[ip("10.0.0.9"), ip("10.0.0.3")],
        )
        .unwrap();
        assert_eq!(d.id, "0123456789abcdef0123456789abcdef");
        assert_eq!(d.name, "Living Room");
        assert_eq!(d.model.as_deref(), Some("Google Nest Mini"));
        assert_eq!(d.addr, "10.0.0.3:8009".parse().unwrap());
    }

    #[test]
    fn cast_from_needs_id_and_v4() {
        let full = "A._googlecast._tcp.local.";
        assert!(cast_from(full, None, Some("A"), None, 8009, &[ip("10.0.0.1")]).is_none());
        assert!(cast_from(full, Some("abc"), Some("A"), None, 8009, &[]).is_none());
    }

    #[test]
    fn name_falls_back_to_instance() {
        let d = cast_from(
            "Kitchen-abc._googlecast._tcp.local.",
            Some("0123456789abcdef0123456789abcdef"),
            None,
            None,
            8009,
            &[ip("10.0.0.1")],
        )
        .unwrap();
        assert_eq!(d.name, "Kitchen-abc");
    }

    #[test]
    fn normalize_cast_id_accepts_dashed_uuid() {
        assert_eq!(
            normalize_cast_id("01234567-89AB-cdef-0123-456789abcdef").as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(normalize_cast_id("nope"), None);
        assert_eq!(normalize_cast_id("0123"), None);
    }

    #[test]
    fn cast_lookup_by_id_or_name() {
        let (d, tx) = Discovery::manual();
        let dev = CastDevice {
            id: "0123456789abcdef0123456789abcdef".into(),
            name: "Living Room".into(),
            model: None,
            addr: "10.0.0.3:8009".parse().unwrap(),
        };
        tx.send(Found {
            casts: vec![dev.clone()],
            snapservers: vec![],
        })
        .unwrap();
        assert_eq!(d.cast("living room"), Some(dev.clone()));
        assert_eq!(d.cast("01234567-89ab-cdef-0123-456789abcdef"), Some(dev));
        assert_eq!(d.cast("kitchen"), None);
        assert_eq!(Discovery::none().found(), Found::default());
    }
}
