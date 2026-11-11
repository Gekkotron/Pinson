//! mDNS discovery of Chromecasts and snapservers on the local network.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::sync::watch;

use super::ssdp;
use super::upnp::xml::DeviceDesc;

const CAST_SERVICE: &str = "_googlecast._tcp.local.";
const SNAP_SERVICE: &str = "_snapcast._tcp.local.";
const RAOP_SERVICE: &str = "_raop._tcp.local.";

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

/// An AirPlay 1 (RAOP) speaker found on the network.
#[derive(Clone, Debug, PartialEq)]
pub struct AirplayDevice {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub addr: SocketAddr,
    /// The speaker needs AES-encrypted audio (`et` lists 1 but not 0).
    pub encrypt: bool,
}

/// Everything discovered so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    /// Sorted by name, then id.
    pub casts: Vec<CastDevice>,
    /// Sorted by host.
    pub snapservers: Vec<SnapServer>,
    /// UPnP media renderers found by SSDP. Sorted by name, then udn.
    pub renderers: Vec<DeviceDesc>,
    /// Sorted by name, then id.
    pub airplay: Vec<AirplayDevice>,
}

/// Removes `-`; `Some(lowercase)` when exactly 32 hex digits remain.
pub fn normalize_cast_id(s: &str) -> Option<String> {
    let id: String = s.chars().filter(|c| *c != '-').collect();
    (id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| id.to_lowercase())
}

/// The address to use from a record's IPv4 addresses: the lowest that is neither
/// loopback nor link-local, else the lowest.
pub(crate) fn pick_v4(v4: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    v4.iter()
        .filter(|ip| !ip.is_loopback() && !ip.is_link_local())
        .min()
        .or_else(|| v4.iter().min())
        .copied()
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
    let ip = pick_v4(v4)?;
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

/// Whether a comma-separated TXT list contains `item`.
fn txt_list_has(list: &str, item: &str) -> bool {
    list.split(',').any(|v| v.trim() == item)
}

/// Builds a speaker from a resolved `_raop._tcp` record; `None` when Pinson can't use it
/// (no IPv4 address, password, no UDP, no ALAC, FairPlay only, other format).
pub(crate) fn airplay_from(
    fullname: &str,
    prop: impl Fn(&str) -> Option<String>,
    port: u16,
    v4: &[Ipv4Addr],
) -> Option<AirplayDevice> {
    let ip = pick_v4(v4)?;
    if prop("pw").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        return None;
    }
    if prop("tp").is_some_and(|v| !txt_list_has(&v, "UDP")) {
        return None;
    }
    if prop("cn").is_some_and(|v| !txt_list_has(&v, "1")) {
        return None;
    }
    let et = prop("et");
    if et
        .as_deref()
        .is_some_and(|v| !txt_list_has(v, "0") && !txt_list_has(v, "1"))
    {
        return None;
    }
    for (key, want) in [("sr", "44100"), ("ss", "16"), ("ch", "2")] {
        if prop(key).is_some_and(|v| v.trim() != want) {
            return None;
        }
    }
    let encrypt = et
        .as_deref()
        .is_some_and(|v| txt_list_has(v, "1") && !txt_list_has(v, "0"));
    let instance = fullname.split("._raop").next().unwrap_or(fullname);
    let (id, name) = match instance.split_once('@') {
        Some((mac, rest)) if mac.len() == 12 && mac.chars().all(|c| c.is_ascii_hexdigit()) => {
            (mac.to_uppercase(), rest.trim().to_string())
        }
        _ => (instance.to_string(), instance.to_string()),
    };
    Some(AirplayDevice {
        id,
        name,
        model: prop("am").filter(|m| !m.is_empty()),
        addr: SocketAddr::new(IpAddr::V4(ip), port),
        encrypt,
    })
}

/// Live view of what mDNS has found.
#[derive(Clone)]
pub struct Discovery {
    rx: watch::Receiver<Found>,
}

#[derive(Default)]
struct State {
    /// By fullname: when it was last resolved (a counter) and the device.
    casts: HashMap<String, (u64, CastDevice)>,
    snaps: HashMap<String, SnapServer>,
    airplay: HashMap<String, AirplayDevice>,
    resolved: u64,
}

impl State {
    fn put_cast(&mut self, fullname: String, dev: CastDevice) {
        self.resolved += 1;
        self.casts.insert(fullname, (self.resolved, dev));
    }

    /// One device per id: an old record that was never removed loses to the latest one.
    fn found(&self) -> Found {
        let mut latest: HashMap<&str, &(u64, CastDevice)> = HashMap::new();
        for entry in self.casts.values() {
            let slot = latest.entry(entry.1.id.as_str()).or_insert(entry);
            if entry.0 > slot.0 {
                *slot = entry;
            }
        }
        let mut casts: Vec<_> = latest.into_values().map(|e| e.1.clone()).collect();
        casts.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        let mut snapservers: Vec<_> = self.snaps.values().cloned().collect();
        snapservers.sort_by_key(|s| s.host);
        let mut airplay: Vec<_> = self.airplay.values().cloned().collect();
        airplay.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        Found {
            casts,
            snapservers,
            renderers: Vec::new(),
            airplay,
        }
    }
}

impl Discovery {
    /// Starts browsing; must run inside the tokio runtime. A failing daemon acts like `none()`.
    pub fn spawn() -> Self {
        let (tx, rx) = watch::channel(Found::default());
        let tx = Arc::new(tx);
        let ssdp_tx = tx.clone();
        ssdp::spawn(move |renderers| {
            ssdp_tx.send_if_modified(|cur| {
                if cur.renderers == renderers {
                    false
                } else {
                    cur.renderers = renderers;
                    true
                }
            });
        });
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("mDNS discovery is off: {e}");
                return Discovery { rx };
            }
        };
        let state = Arc::new(Mutex::new(State::default()));
        for service in [CAST_SERVICE, SNAP_SERVICE, RAOP_SERVICE] {
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
                                    Some(d) => st.put_cast(info.fullname.clone(), d),
                                    None => {
                                        st.casts.remove(&info.fullname);
                                    }
                                }
                            } else if service == RAOP_SERVICE {
                                let dev = airplay_from(
                                    &info.fullname,
                                    |k| info.get_property_val_str(k).map(str::to_string),
                                    info.port,
                                    &v4,
                                );
                                match dev {
                                    Some(d) => {
                                        st.airplay.insert(info.fullname.clone(), d);
                                    }
                                    None => {
                                        st.airplay.remove(&info.fullname);
                                    }
                                }
                            } else if let Some(ip) = pick_v4(&v4) {
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
                                        host: IpAddr::V4(ip),
                                    },
                                );
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            st.casts.remove(&fullname);
                            st.snaps.remove(&fullname);
                            st.airplay.remove(&fullname);
                        }
                        _ => continue,
                    }
                    let found = st.found();
                    drop(st);
                    tx.send_if_modified(|cur| {
                        let found = Found {
                            renderers: cur.renderers.clone(),
                            ..found
                        };
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

    /// A UPnP renderer by UDN (`uuid:` prefix and case ignored) or by name (case-insensitive).
    pub fn renderer(&self, key: &str) -> Option<DeviceDesc> {
        let found = self.rx.borrow();
        let key = key.trim();
        let udn = key
            .get(..5)
            .filter(|p| p.eq_ignore_ascii_case("uuid:"))
            .map_or(key, |_| &key[5..]);
        found
            .renderers
            .iter()
            .find(|d| d.udn.eq_ignore_ascii_case(udn))
            .or_else(|| {
                found
                    .renderers
                    .iter()
                    .find(|d| d.name.eq_ignore_ascii_case(key))
            })
            .cloned()
    }

    /// An AirPlay speaker by id (case, `:` and `-` ignored) or by name (case-insensitive).
    pub fn airplay(&self, key: &str) -> Option<AirplayDevice> {
        let found = self.rx.borrow();
        let key = key.trim();
        let norm = |s: &str| -> String {
            s.chars()
                .filter(|c| *c != ':' && *c != '-')
                .collect::<String>()
                .to_uppercase()
        };
        let want = norm(key);
        found
            .airplay
            .iter()
            .find(|d| norm(&d.id) == want)
            .or_else(|| {
                found
                    .airplay
                    .iter()
                    .find(|d| d.name.eq_ignore_ascii_case(key))
            })
            .cloned()
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
    fn prefers_a_lan_address() {
        let lan = [
            ip("127.0.0.1"),
            ip("169.254.3.4"),
            ip("10.0.0.9"),
            ip("10.0.0.3"),
        ];
        assert_eq!(pick_v4(&lan), Some(ip("10.0.0.3")));
        assert_eq!(
            pick_v4(&[ip("169.254.3.4"), ip("127.0.0.1")]),
            Some(ip("127.0.0.1"))
        );
        assert_eq!(pick_v4(&[]), None);
        let d = cast_from(
            "A._googlecast._tcp.local.",
            Some("0123456789abcdef0123456789abcdef"),
            None,
            None,
            8009,
            &[ip("127.0.0.1"), ip("192.168.1.20")],
        )
        .unwrap();
        assert_eq!(d.addr, "192.168.1.20:8009".parse().unwrap());
    }

    #[test]
    fn found_dedupes_casts_by_id() {
        let dev = |addr: &str| CastDevice {
            id: "0123456789abcdef0123456789abcdef".into(),
            name: "Living Room".into(),
            model: None,
            addr: addr.parse().unwrap(),
        };
        let mut st = State::default();
        st.put_cast("old._googlecast._tcp.local.".into(), dev("10.0.0.3:8009"));
        st.put_cast("new._googlecast._tcp.local.".into(), dev("10.0.0.4:8009"));
        assert_eq!(st.found().casts, vec![dev("10.0.0.4:8009")]);
        // Resolved again: the old record is now the latest.
        st.put_cast("old._googlecast._tcp.local.".into(), dev("10.0.0.5:8009"));
        assert_eq!(st.found().casts, vec![dev("10.0.0.5:8009")]);
        st.casts.remove("old._googlecast._tcp.local.");
        assert_eq!(st.found().casts, vec![dev("10.0.0.4:8009")]);
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
            renderers: vec![],
            airplay: vec![],
        })
        .unwrap();
        assert_eq!(d.cast("living room"), Some(dev.clone()));
        assert_eq!(d.cast("01234567-89ab-cdef-0123-456789abcdef"), Some(dev));
        assert_eq!(d.cast("kitchen"), None);
        assert_eq!(Discovery::none().found(), Found::default());
    }

    #[test]
    fn renderer_lookup_by_udn_or_name() {
        let xml = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/upnp/tv.xml"
        ))
        .unwrap();
        let desc = crate::output::upnp::xml::parse_description(
            &xml,
            "http://10.0.0.3:49152/description.xml",
        )
        .unwrap();
        let (d, tx) = Discovery::manual();
        tx.send(Found {
            renderers: vec![desc.clone()],
            ..Found::default()
        })
        .unwrap();
        assert_eq!(
            d.renderer("uuid:4D696E69-1234-4abc-8def-0123456789ab"),
            Some(desc.clone())
        );
        assert_eq!(d.renderer("55mled910/12(10.0.0.3)"), Some(desc));
        assert_eq!(d.renderer("kitchen"), None);
    }

    fn txt(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    const RAOP: &str = "0A1B2C3D4E5F@Living Room._raop._tcp.local.";

    #[test]
    fn airplay_from_reads_txt() {
        let t = txt(&[
            ("et", "0,1"),
            ("cn", "0,1,2,3"),
            ("tp", "TCP,UDP"),
            ("am", "AirPort10,115"),
            ("sr", "44100"),
            ("ss", "16"),
            ("ch", "2"),
            ("pw", "false"),
        ]);
        let d = airplay_from(RAOP, t, 5000, &[ip("10.0.0.9"), ip("10.0.0.3")]).unwrap();
        assert_eq!(d.id, "0A1B2C3D4E5F");
        assert_eq!(d.name, "Living Room");
        assert_eq!(d.model.as_deref(), Some("AirPort10,115"));
        assert_eq!(d.addr, "10.0.0.3:5000".parse().unwrap());
        assert!(!d.encrypt);
    }

    #[test]
    fn airplay_from_needs_aes_when_et_is_1_only() {
        let v4 = [ip("10.0.0.3")];
        let d = airplay_from(RAOP, txt(&[("et", "1")]), 5000, &v4).unwrap();
        assert!(d.encrypt);
        let d = airplay_from(RAOP, txt(&[("et", "1,3")]), 5000, &v4).unwrap();
        assert!(d.encrypt);
        let d = airplay_from(RAOP, txt(&[]), 5000, &v4).unwrap();
        assert!(!d.encrypt);
    }

    #[test]
    fn airplay_from_skips_unusable() {
        let v4 = [ip("10.0.0.3")];
        for pairs in [
            vec![("pw", "true")],
            vec![("et", "3,5")],
            vec![("cn", "2,3")],
            vec![("tp", "TCP")],
            vec![("sr", "48000")],
            vec![("ss", "24")],
            vec![("ch", "6")],
        ] {
            assert!(
                airplay_from(RAOP, txt(&pairs), 5000, &v4).is_none(),
                "{pairs:?}"
            );
        }
        assert!(airplay_from(RAOP, txt(&[]), 5000, &[]).is_none());
    }

    #[test]
    fn airplay_from_without_mac_prefix() {
        let d = airplay_from(
            "Kitchen._raop._tcp.local.",
            txt(&[]),
            5000,
            &[ip("10.0.0.3")],
        )
        .unwrap();
        assert_eq!(d.id, "Kitchen");
        assert_eq!(d.name, "Kitchen");
        assert_eq!(d.model, None);
    }

    #[test]
    fn airplay_lookup_by_id_or_name() {
        let (d, tx) = Discovery::manual();
        let dev = AirplayDevice {
            id: "0A1B2C3D4E5F".into(),
            name: "Living Room".into(),
            model: None,
            addr: "10.0.0.3:5000".parse().unwrap(),
            encrypt: false,
        };
        tx.send(Found {
            airplay: vec![dev.clone()],
            ..Found::default()
        })
        .unwrap();
        assert_eq!(d.airplay("0a:1b:2c:3d:4e:5f"), Some(dev.clone()));
        assert_eq!(d.airplay("0A-1B-2C-3D-4E-5F"), Some(dev.clone()));
        assert_eq!(d.airplay("living room"), Some(dev));
        assert_eq!(d.airplay("kitchen"), None);
    }
}
