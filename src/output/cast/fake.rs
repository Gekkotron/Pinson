//! A scripted Chromecast for tests: serves the Cast protocol over `tokio::io::duplex`
//! and records every request it gets.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{DuplexStream, ReadHalf, split};
use tokio::sync::{Notify, mpsc};

use super::proto::*;
use super::session::CastConnector;
use super::tls::CastLink;

/// A request the fake received: (destination, namespace, JSON payload).
pub(crate) type Request = (String, String, Value);

struct State {
    apps: Vec<Value>,
    volume: Value,
    next_media: i64,
    /// The current media session and its player state.
    media: Option<(i64, &'static str)>,
    fail_next_load: bool,
    fail_next_launch: bool,
    /// LOADs are recorded but not answered; `answer_load` answers them in order.
    hold_loads: bool,
    held_loads: Vec<u64>,
    /// Accept each connection, then close it at once.
    drop_on_accept: bool,
    connect_times: Vec<tokio::time::Instant>,
    silent: bool,
    launches: usize,
    refuse: bool,
    connects: usize,
    closed: usize,
    push: Option<mpsc::UnboundedSender<CastMessage>>,
    kill: Option<Arc<Notify>>,
}

struct Inner {
    requests: Mutex<Vec<Request>>,
    notify: Notify,
    state: Mutex<State>,
}

/// The fake device. Clones share state.
#[derive(Clone)]
pub(crate) struct FakeReceiver {
    inner: Arc<Inner>,
}

/// What a session connects through; each `connect` serves a fresh pipe.
pub(crate) struct FakeConnector {
    rx: FakeReceiver,
}

/// The app a LAUNCH starts: sessions `s1`, `s2`, ... with transports `t1`, `t2`, ...
fn app(n: usize) -> Value {
    json!({"appId": APP_ID, "sessionId": format!("s{n}"), "transportId": format!("t{n}"),
           "displayName": "Default Media Receiver"})
}

/// The transport media messages come from: the running app's, else `t1`.
fn transport(st: &State) -> String {
    st.apps
        .first()
        .and_then(|a| a["transportId"].as_str())
        .unwrap_or("t1")
        .to_owned()
}

impl FakeReceiver {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                requests: Mutex::new(Vec::new()),
                notify: Notify::new(),
                state: Mutex::new(State {
                    apps: Vec::new(),
                    volume: json!({"level": 0.5, "muted": false}),
                    next_media: 0,
                    media: None,
                    fail_next_load: false,
                    fail_next_launch: false,
                    hold_loads: false,
                    held_loads: Vec::new(),
                    drop_on_accept: false,
                    connect_times: Vec::new(),
                    silent: false,
                    launches: 0,
                    refuse: false,
                    connects: 0,
                    closed: 0,
                    push: None,
                    kill: None,
                }),
            }),
        }
    }

    pub(crate) fn connector(&self) -> Arc<FakeConnector> {
        Arc::new(FakeConnector { rx: self.clone() })
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.inner.requests.lock().unwrap().clone()
    }

    /// Requests whose payload `type` is `ty`, in order.
    pub(crate) fn requests_of(&self, ty: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.2["type"] == ty)
            .collect()
    }

    pub(crate) fn count(&self, ty: &str) -> usize {
        self.requests_of(ty).len()
    }

    /// Waits for the `n`th request of type `ty` and returns it.
    pub(crate) async fn wait_for_n(&self, ty: &str, n: usize) -> Request {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(r) = self.requests_of(ty).into_iter().nth(n - 1) {
                return r;
            }
            if tokio::time::timeout(Duration::from_secs(30), notified)
                .await
                .is_err()
            {
                panic!(
                    "no {n}th {ty} request; got {:?}",
                    self.requests()
                        .iter()
                        .map(|r| r.2["type"].clone())
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    pub(crate) async fn wait_for(&self, ty: &str) -> Request {
        self.wait_for_n(ty, 1).await
    }

    /// An unsolicited message to the sender (`requestId` 0 unless given).
    pub(crate) fn push(&self, namespace: &str, mut value: Value) {
        if value.get("requestId").is_none() {
            value["requestId"] = json!(0);
        }
        let st = self.inner.state.lock().unwrap();
        let source = if namespace == NS_MEDIA {
            transport(&st)
        } else {
            PLATFORM_ID.to_owned()
        };
        let msg = CastMessage {
            source,
            destination: SENDER_ID.into(),
            namespace: namespace.into(),
            payload: value.to_string(),
        };
        st.push
            .as_ref()
            .expect("push without a connection")
            .send(msg)
            .ok();
    }

    /// An unsolicited `MEDIA_STATUS` for `media_session_id`.
    pub(crate) fn push_media_status(
        &self,
        media_session_id: i64,
        player_state: &str,
        idle_reason: Option<&str>,
        current_time: f64,
    ) {
        let mut e = json!({"mediaSessionId": media_session_id, "playerState": player_state,
                           "currentTime": current_time});
        if let Some(r) = idle_reason {
            e["idleReason"] = json!(r);
        }
        self.push(NS_MEDIA, json!({"type": "MEDIA_STATUS", "status": [e]}));
    }

    /// The most recently assigned `mediaSessionId` (0 before any LOAD).
    pub(crate) fn media_session_id(&self) -> i64 {
        self.inner.state.lock().unwrap().media.map_or(0, |m| m.0)
    }

    /// Closes the current connection from the device side.
    pub(crate) fn drop_link(&self) {
        if let Some(k) = self.inner.state.lock().unwrap().kill.take() {
            k.notify_one();
        }
    }

    /// Replaces the `applications` that `RECEIVER_STATUS` reports.
    pub(crate) fn set_apps(&self, apps: Vec<Value>) {
        self.inner.state.lock().unwrap().apps = apps;
    }

    /// Answers the next LOAD with `LOAD_FAILED`.
    pub(crate) fn fail_next_load(&self) {
        self.inner.state.lock().unwrap().fail_next_load = true;
    }

    /// Answers the next LAUNCH with `LAUNCH_ERROR`.
    pub(crate) fn fail_next_launch(&self) {
        self.inner.state.lock().unwrap().fail_next_launch = true;
    }

    /// While set, LOADs get no answer until `answer_load`.
    pub(crate) fn hold_loads(&self, hold: bool) {
        self.inner.state.lock().unwrap().hold_loads = hold;
    }

    /// Answers the oldest held LOAD (PLAYING, a new media session); returns that session.
    pub(crate) fn answer_load(&self) -> i64 {
        let mut st = self.inner.state.lock().unwrap();
        assert!(!st.held_loads.is_empty(), "no held LOAD");
        let rid = st.held_loads.remove(0);
        st.next_media += 1;
        st.media = Some((st.next_media, "PLAYING"));
        let msg = CastMessage {
            source: transport(&st),
            destination: SENDER_ID.into(),
            namespace: NS_MEDIA.into(),
            payload: media_status(&st, rid).to_string(),
        };
        st.push.as_ref().expect("no connection").send(msg).ok();
        st.next_media
    }

    /// Sets the player state the current media session reports (e.g. PAUSED from the device).
    pub(crate) fn set_media_state(&self, state: &'static str) {
        if let Some(m) = self.inner.state.lock().unwrap().media.as_mut() {
            m.1 = state;
        }
    }

    /// Accept every connection and close it right away (a port that isn't a Cast receiver).
    pub(crate) fn set_drop_on_accept(&self, on: bool) {
        self.inner.state.lock().unwrap().drop_on_accept = on;
    }

    /// When each connection attempt started.
    pub(crate) fn connect_times(&self) -> Vec<tokio::time::Instant> {
        self.inner.state.lock().unwrap().connect_times.clone()
    }

    /// While silent the fake records requests but answers nothing (not even PING).
    pub(crate) fn set_silent(&self, silent: bool) {
        self.inner.state.lock().unwrap().silent = silent;
    }

    pub(crate) fn set_refuse(&self, refuse: bool) {
        self.inner.state.lock().unwrap().refuse = refuse;
    }

    /// Connection attempts so far, refused ones included.
    pub(crate) fn connects(&self) -> usize {
        self.inner.state.lock().unwrap().connects
    }

    /// Waits until `n` connections have ended (EOF or `drop_link`).
    pub(crate) async fn wait_closed(&self, n: usize) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.state.lock().unwrap().closed >= n {
                return;
            }
            tokio::time::timeout(Duration::from_secs(30), notified)
                .await
                .expect("connection never closed");
        }
    }

    fn handle(&self, msg: &CastMessage) -> Vec<CastMessage> {
        let v: Value = serde_json::from_str(&msg.payload).unwrap_or(Value::Null);
        self.inner.requests.lock().unwrap().push((
            msg.destination.clone(),
            msg.namespace.clone(),
            v.clone(),
        ));
        self.inner.notify.notify_waiters();
        let rid = v["requestId"].as_u64().unwrap_or(0);
        let ty = v["type"].as_str().unwrap_or("");
        let mut st = self.inner.state.lock().unwrap();
        if st.silent {
            return Vec::new();
        }
        let t = transport(&st);
        let (source, ns, reply) = match (msg.namespace.as_str(), ty) {
            (NS_HEARTBEAT, "PING") => (
                PLATFORM_ID.to_owned(),
                NS_HEARTBEAT,
                json!({"type": "PONG"}),
            ),
            (NS_RECEIVER, "GET_STATUS") => (
                PLATFORM_ID.to_owned(),
                NS_RECEIVER,
                receiver_status(&st, rid),
            ),
            (NS_RECEIVER, "LAUNCH") => {
                if std::mem::take(&mut st.fail_next_launch) {
                    (
                        PLATFORM_ID.to_owned(),
                        NS_RECEIVER,
                        json!({"type": "LAUNCH_ERROR", "requestId": rid, "reason": "NOT_ALLOWED"}),
                    )
                } else {
                    st.launches += 1;
                    st.apps = vec![app(st.launches)];
                    (
                        PLATFORM_ID.to_owned(),
                        NS_RECEIVER,
                        receiver_status(&st, rid),
                    )
                }
            }
            (NS_RECEIVER, "STOP") => {
                st.apps.clear();
                st.media = None;
                (
                    PLATFORM_ID.to_owned(),
                    NS_RECEIVER,
                    receiver_status(&st, rid),
                )
            }
            (NS_RECEIVER, "SET_VOLUME") => {
                for key in ["level", "muted"] {
                    if !v["volume"][key].is_null() {
                        st.volume[key] = v["volume"][key].clone();
                    }
                }
                (
                    PLATFORM_ID.to_owned(),
                    NS_RECEIVER,
                    receiver_status(&st, rid),
                )
            }
            (NS_MEDIA, "LOAD") if st.hold_loads => {
                st.held_loads.push(rid);
                return Vec::new();
            }
            (NS_MEDIA, "LOAD") => {
                if std::mem::take(&mut st.fail_next_load) {
                    (
                        t,
                        NS_MEDIA,
                        json!({"type": "LOAD_FAILED", "requestId": rid}),
                    )
                } else {
                    st.next_media += 1;
                    st.media = Some((st.next_media, "PLAYING"));
                    (t, NS_MEDIA, media_status(&st, rid))
                }
            }
            (NS_MEDIA, "PAUSE") | (NS_MEDIA, "PLAY") | (NS_MEDIA, "STOP") => {
                if let Some(m) = st.media.as_mut() {
                    m.1 = match ty {
                        "PAUSE" => "PAUSED",
                        "PLAY" => "PLAYING",
                        _ => "IDLE",
                    };
                }
                (t, NS_MEDIA, media_status(&st, rid))
            }
            (NS_MEDIA, "GET_STATUS") => (t, NS_MEDIA, media_status(&st, rid)),
            _ => return Vec::new(),
        };
        vec![CastMessage {
            source,
            destination: SENDER_ID.into(),
            namespace: ns.into(),
            payload: reply.to_string(),
        }]
    }

    async fn serve(
        self,
        mut rd: ReadHalf<DuplexStream>,
        kill: Arc<Notify>,
        tx: mpsc::UnboundedSender<CastMessage>,
    ) {
        loop {
            tokio::select! {
                _ = kill.notified() => break,
                r = read_frame(&mut rd) => match r {
                    Ok(msg) => for reply in self.handle(&msg) { tx.send(reply).ok(); },
                    Err(_) => break,
                },
            }
        }
        let mut st = self.inner.state.lock().unwrap();
        if st.push.as_ref().is_some_and(|p| p.same_channel(&tx)) {
            st.push = None;
        }
        st.closed += 1;
        drop(st);
        self.inner.notify.notify_waiters();
    }
}

fn receiver_status(st: &State, rid: u64) -> Value {
    let mut status = json!({"volume": st.volume});
    if !st.apps.is_empty() {
        status["applications"] = Value::Array(st.apps.clone());
    }
    json!({"type": "RECEIVER_STATUS", "requestId": rid, "status": status})
}

fn media_status(st: &State, rid: u64) -> Value {
    let entries: Vec<Value> = st
        .media
        .iter()
        .map(|&(id, state)| {
            let mut e = json!({"mediaSessionId": id, "playerState": state, "currentTime": 0.0});
            if state == "IDLE" {
                e["idleReason"] = json!("CANCELLED");
            }
            e
        })
        .collect();
    json!({"type": "MEDIA_STATUS", "requestId": rid, "status": entries})
}

impl CastConnector for FakeConnector {
    fn target(&self) -> String {
        "fake-chromecast".into()
    }

    fn connect(&self) -> BoxFuture<'static, io::Result<CastLink>> {
        let rx = self.rx.clone();
        Box::pin(async move {
            {
                let mut st = rx.inner.state.lock().unwrap();
                st.connects += 1;
                st.connect_times.push(tokio::time::Instant::now());
                if st.refuse {
                    return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"));
                }
                if st.drop_on_accept {
                    let (client, server) = tokio::io::duplex(64 * 1024);
                    drop(server);
                    return Ok(CastLink {
                        io: Box::new(client),
                        local_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    });
                }
            }
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (out_tx, mut out_rx) = mpsc::unbounded_channel::<CastMessage>();
            let kill = Arc::new(Notify::new());
            {
                let mut st = rx.inner.state.lock().unwrap();
                st.push = Some(out_tx.clone());
                st.kill = Some(kill.clone());
            }
            let (srv_rd, mut srv_wr) = split(server);
            // Writer: everything the fake says goes through `out_rx`.
            tokio::spawn(async move {
                while let Some(m) = out_rx.recv().await {
                    if write_frame(&mut srv_wr, &m).await.is_err() {
                        break;
                    }
                }
            });
            tokio::spawn(rx.clone().serve(srv_rd, kill, out_tx));
            Ok(CastLink {
                io: Box::new(client),
                local_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            })
        })
    }
}
