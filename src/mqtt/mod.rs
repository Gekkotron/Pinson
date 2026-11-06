//! Pinson as an MQTT media player (Home Assistant-style discovery, as Nidoo speaks it).
//!
//! The `rumqttc` glue is kept thin: one task polls the event loop and only forwards what it
//! sees; another owns everything else (commands, state publishing, colours).

pub mod commands;
pub mod protocol;
pub mod state;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, LastWill, MqttOptions, Outgoing, QoS};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::api::Services;
use crate::config::MqttConfig;
use crate::player::StateView;
use commands::{Outcome, dispatch};
use protocol::{OFFLINE, ONLINE, Topics, discovery_payload, parse_command};
use state::{Input, Tracker, color_for};

/// Player changes within this window go out as one state message.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Wait between reconnect attempts while the broker is unreachable.
const RETRY: Duration = Duration::from_secs(2);
/// The longest `shutdown` may take, broker or not.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// Requests queued for the event loop; publishes past this are dropped, never awaited.
const REQUEST_CAP: usize = 64;

/// What to publish on every (re)connect, as `(topic, payload, retain)`: the discovery
/// config, then `online`. The current state follows separately, forced.
pub fn on_connected(cfg: &MqttConfig, topics: &Topics) -> Vec<(String, Vec<u8>, bool)> {
    let discovery = serde_json::to_vec(&discovery_payload(cfg, topics))
        .expect("a JSON value always serialises");
    vec![
        (topics.discovery.clone(), discovery, true),
        (
            topics.availability.clone(),
            ONLINE.as_bytes().to_vec(),
            true,
        ),
    ]
}

/// The running MQTT client; see [`spawn`].
pub struct MqttHandle {
    client: AsyncClient,
    availability: String,
    /// Whether the broker connection is up right now.
    online: Arc<AtomicBool>,
    main: JoinHandle<()>,
    poll: JoinHandle<()>,
}

impl MqttHandle {
    /// Publishes `offline` (retained) and disconnects. Takes at most about a second, even
    /// when the broker is down.
    pub async fn shutdown(self) {
        self.main.abort();
        // try_*: a full request queue (broker down for long) must not hang the shutdown.
        let sent = self
            .client
            .try_publish(&self.availability, QoS::AtLeastOnce, true, OFFLINE)
            .and_then(|()| self.client.try_disconnect());
        if let Err(e) = sent {
            tracing::debug!("mqtt: could not queue offline: {e}");
        }
        let mut poll = self.poll;
        if !self.online.load(Ordering::Relaxed) {
            // Nothing can reach the broker; the last will says `offline` once it returns.
            poll.abort();
            return;
        }
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut poll)
            .await
            .is_err()
        {
            tracing::debug!("mqtt: broker did not take the disconnect in time");
            poll.abort();
        }
    }
}

/// Connects to the broker in the background and serves the media player contract until
/// [`MqttHandle::shutdown`]. Never blocks: an unreachable broker is retried quietly.
pub fn spawn(cfg: MqttConfig, app: Services) -> MqttHandle {
    let topics = Topics::new(&cfg);
    let mut options = MqttOptions::new(format!("pinson-{}", cfg.root), &cfg.host, cfg.port);
    options.set_keep_alive(KEEP_ALIVE);
    options.set_last_will(LastWill::new(
        &topics.availability,
        OFFLINE,
        QoS::AtLeastOnce,
        true,
    ));
    if let Some(user) = &cfg.user {
        options.set_credentials(user, cfg.password.clone().unwrap_or_default());
    }
    let (client, eventloop) = AsyncClient::new(options, REQUEST_CAP);
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let online = Arc::new(AtomicBool::new(false));
    let poll = tokio::spawn(poll_loop(eventloop, events_tx, online.clone()));
    let main = tokio::spawn(
        Publisher {
            cfg,
            topics: topics.clone(),
            client: client.clone(),
            app,
            tracker: Tracker::new(),
            connected: false,
            color_asked: None,
            color_warmed: None,
        }
        .run(events_rx),
    );
    MqttHandle {
        client,
        availability: topics.availability,
        online,
        main,
        poll,
    }
}

/// What the polling task forwards.
enum BrokerEvent {
    Connected,
    Disconnected,
    Message { topic: String, payload: Vec<u8> },
}

/// Internal wake-ups for the publisher.
enum Wake {
    /// A command made the player act: publish the state now, even if unchanged.
    Flush,
    /// A cover colour finished computing: look again.
    ColorReady,
}

/// Polls the `rumqttc` event loop (which reconnects on its own) and forwards events.
async fn poll_loop(
    mut eventloop: EventLoop,
    events: mpsc::UnboundedSender<BrokerEvent>,
    online: Arc<AtomicBool>,
) {
    let mut down = false;
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                if down {
                    tracing::info!("mqtt: reconnected");
                } else {
                    tracing::info!("mqtt: connected");
                }
                down = false;
                online.store(true, Ordering::Relaxed);
                let _ = events.send(BrokerEvent::Connected);
            }
            Ok(Event::Incoming(Incoming::Publish(p))) => {
                let _ = events.send(BrokerEvent::Message {
                    topic: p.topic,
                    payload: p.payload.to_vec(),
                });
            }
            Ok(Event::Outgoing(Outgoing::Disconnect)) => return,
            Ok(_) => {}
            Err(e) => {
                online.store(false, Ordering::Relaxed);
                if down {
                    tracing::debug!("mqtt: still unreachable: {e}");
                } else {
                    tracing::warn!("mqtt: broker unreachable, retrying every 2 s: {e}");
                    down = true;
                    let _ = events.send(BrokerEvent::Disconnected);
                }
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Owns everything but the polling: commands, the debounced state, cover colours.
struct Publisher {
    cfg: MqttConfig,
    topics: Topics,
    client: AsyncClient,
    app: Services,
    tracker: Tracker,
    connected: bool,
    /// The last current-song artwork whose colour was requested (one attempt per song, so an
    /// upstream error cannot turn into a fetch loop).
    color_asked: Option<String>,
    color_warmed: Option<String>,
}

impl Publisher {
    async fn run(mut self, mut events: mpsc::UnboundedReceiver<BrokerEvent>) {
        let (wake_tx, mut wake_rx) = mpsc::unbounded_channel();
        let mut player = self.app.player.subscribe();
        let mut deadline: Option<Instant> = None;
        loop {
            let debounce = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                event = events.recv() => match event {
                    Some(BrokerEvent::Connected) => self.on_connack(&wake_tx),
                    Some(BrokerEvent::Disconnected) => self.connected = false,
                    Some(BrokerEvent::Message { topic, payload }) => {
                        self.on_message(&topic, &payload, &wake_tx);
                    }
                    None => return,
                },
                changed = player.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    deadline.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                }
                () = debounce => {
                    deadline = None;
                    self.evaluate(false, &wake_tx);
                }
                Some(wake) = wake_rx.recv() => match wake {
                    Wake::Flush => self.evaluate(true, &wake_tx),
                    Wake::ColorReady => self.evaluate(false, &wake_tx),
                },
            }
        }
    }

    fn on_connack(&mut self, wake: &mpsc::UnboundedSender<Wake>) {
        self.connected = true;
        if let Err(e) = self
            .client
            .try_subscribe(&self.topics.command, QoS::AtLeastOnce)
        {
            tracing::warn!("mqtt: could not subscribe to {}: {e}", self.topics.command);
        }
        for (topic, payload, retain) in on_connected(&self.cfg, &self.topics) {
            self.publish(&topic, payload, retain);
        }
        self.evaluate(true, wake);
    }

    fn on_message(&self, topic: &str, payload: &[u8], wake: &mpsc::UnboundedSender<Wake>) {
        if topic != self.topics.command {
            return;
        }
        let cmd = match parse_command(payload) {
            Ok(cmd) => cmd,
            Err(e) => {
                tracing::warn!("mqtt: ignored command on {topic}: {e}");
                return;
            }
        };
        tracing::info!("mqtt: command {cmd:?}");
        let app = self.app.clone();
        let wake = wake.clone();
        // A search can be slow: never hold up the loop for it.
        tokio::spawn(async move {
            if dispatch(&app, cmd).await == Outcome::Echo {
                let _ = wake.send(Wake::Flush);
            }
        });
    }

    /// Projects the player's current state and publishes it if it changed (or `force`).
    fn evaluate(&mut self, force: bool, wake: &mpsc::UnboundedSender<Wake>) {
        let now = Instant::now();
        let state = self.app.player.state();
        let view = state.view(now);
        self.fetch_colors(&view, wake);
        if !self.connected {
            // The forced publish on the next ConnAck covers whatever happens meanwhile.
            return;
        }
        let colors = &self.app.colors;
        let input = Input {
            view: &view,
            moves_in: state.moves_in(now),
            now,
            wall: Utc::now(),
            color: color_for(&view, |url| colors.cached(url)),
        };
        if let Some(out) = self.tracker.update(input, force) {
            match serde_json::to_vec(&out) {
                Ok(payload) => self.publish(&self.topics.state, payload, true),
                Err(e) => tracing::warn!("mqtt: state not serialisable: {e}"),
            }
        }
    }

    /// Starts computing the current song's cover colour (re-evaluating when it lands) and
    /// warms the next song's, each at most once per song.
    fn fetch_colors(&mut self, view: &StateView, wake: &mpsc::UnboundedSender<Wake>) {
        let current = view
            .current
            .as_ref()
            .and_then(|e| e.song.artwork_url.clone());
        if let Some(url) = current
            && self.color_asked.as_deref() != Some(url.as_str())
            && self.app.colors.cached(&url).is_none()
        {
            self.color_asked = Some(url.clone());
            let colors = self.app.colors.clone();
            let wake = wake.clone();
            tokio::spawn(async move {
                colors.get(&url).await;
                let _ = wake.send(Wake::ColorReady);
            });
        }
        let next = view
            .upcoming
            .first()
            .and_then(|e| e.song.artwork_url.clone());
        if let Some(url) = next
            && self.color_warmed.as_deref() != Some(url.as_str())
            && self.app.colors.cached(&url).is_none()
        {
            self.color_warmed = Some(url.clone());
            let colors = self.app.colors.clone();
            tokio::spawn(async move {
                colors.get(&url).await;
            });
        }
    }

    fn publish(&self, topic: &str, payload: Vec<u8>, retain: bool) {
        if let Err(e) = self
            .client
            .try_publish(topic, QoS::AtLeastOnce, retain, payload)
        {
            tracing::warn!("mqtt: could not queue a message for {topic}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MqttConfig {
        MqttConfig {
            host: "localhost".into(),
            port: 1883,
            user: None,
            password: None,
            discovery_prefix: "homeassistant".into(),
            root: "pinson".into(),
        }
    }

    #[test]
    fn on_connected_publishes_discovery_and_online() {
        let c = cfg();
        let t = Topics::new(&c);
        let list = on_connected(&c, &t);
        assert_eq!(
            list,
            vec![
                (
                    "homeassistant/media_player/pinson/player/config".to_string(),
                    serde_json::to_vec(&discovery_payload(&c, &t)).unwrap(),
                    true,
                ),
                (
                    "pinson/status".to_string(),
                    ONLINE.as_bytes().to_vec(),
                    true
                ),
            ]
        );
    }
}
