//! The last WARN and ERROR log events since start, for the Health page.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Serialize, Serializer};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Most entries kept; the oldest is dropped first.
pub const CAP: usize = 50;

/// One problem, with how many times in a row it happened.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogEntry {
    /// When it last happened.
    #[serde(serialize_with = "rfc3339")]
    pub at: DateTime<Utc>,
    /// `"warn"` or `"error"`.
    pub level: &'static str,
    pub target: String,
    pub message: String,
    /// Consecutive repeats, 1 for a single event.
    pub count: u32,
}

fn rfc3339<S: Serializer>(at: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&at.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// A shared, capped list of recent problems. Add it to the tracing registry as a layer.
#[derive(Clone, Default)]
pub struct ErrorLog {
    entries: Arc<Mutex<VecDeque<LogEntry>>>,
}

impl ErrorLog {
    /// Records a problem; a repeat of the newest entry bumps its count instead.
    pub fn push(&self, level: &'static str, target: &str, message: String) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(last) = entries.back_mut()
            && last.level == level
            && last.target == target
            && last.message == message
        {
            last.count += 1;
            last.at = Utc::now();
            return;
        }
        entries.push_back(LogEntry {
            at: Utc::now(),
            level,
            target: target.to_owned(),
            message,
            count: 1,
        });
        if entries.len() > CAP {
            entries.pop_front();
        }
    }

    /// The entries, newest first.
    pub fn entries(&self) -> Vec<LogEntry> {
        self.entries.lock().unwrap().iter().rev().cloned().collect()
    }
}

/// Builds the message from an event's `message` field, then ` key=value` for the others.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for ErrorLog {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = match *meta.level() {
            Level::ERROR => "error",
            Level::WARN => "warn",
            _ => return,
        };
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.push(
            level,
            meta.target(),
            format!("{}{}", visitor.message, visitor.fields),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn repeats_collapse_and_newest_comes_first() {
        let log = ErrorLog::default();
        log.push("warn", "a", "one".into());
        log.push("warn", "a", "two".into());
        log.push("warn", "a", "two".into());
        let entries = log.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].message.as_str(), entries[0].count), ("two", 2));
        assert_eq!((entries[1].message.as_str(), entries[1].count), ("one", 1));
    }

    #[test]
    fn same_message_with_another_level_or_target_is_a_new_entry() {
        let log = ErrorLog::default();
        log.push("warn", "a", "x".into());
        log.push("error", "a", "x".into());
        log.push("error", "b", "x".into());
        assert_eq!(log.entries().len(), 3);
    }

    #[test]
    fn a_repeat_is_only_collapsed_into_the_newest_entry() {
        let log = ErrorLog::default();
        log.push("warn", "a", "x".into());
        log.push("warn", "a", "y".into());
        log.push("warn", "a", "x".into());
        assert_eq!(log.entries().len(), 3);
    }

    #[test]
    fn keeps_the_newest_fifty() {
        let log = ErrorLog::default();
        for i in 0..60 {
            log.push("warn", "a", format!("m{i}"));
        }
        let entries = log.entries();
        assert_eq!(entries.len(), CAP);
        assert_eq!(entries[0].message, "m59");
        assert_eq!(entries[CAP - 1].message, "m10");
    }

    #[test]
    fn layer_records_only_warn_and_error_with_fields() {
        let log = ErrorLog::default();
        let subscriber = tracing_subscriber::registry().with(log.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("fine");
            tracing::warn!(song = "abc", "slow {}", 3);
            tracing::error!("bad");
        });
        let entries = log.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level, "error");
        assert_eq!(entries[0].message, "bad");
        assert_eq!(entries[1].level, "warn");
        assert_eq!(entries[1].message, "slow 3 song=\"abc\"");
        assert!(entries[1].target.starts_with("pinson"));
    }

    #[test]
    fn serializes_time_as_utc_seconds() {
        let log = ErrorLog::default();
        log.push("warn", "a", "x".into());
        let v = serde_json::to_value(&log.entries()[0]).unwrap();
        let at = v["at"].as_str().unwrap();
        assert!(at.ends_with('Z') && !at.contains('.'), "{at}");
        assert_eq!(v["count"], 1);
    }
}
