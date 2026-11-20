//! The outputs and playlists Nidoo may pick from, as plain `{id, name}` menus. Pure: no I/O.
//!
//! An output's id is `OutputOption.id` when the option stands for one discovered device
//! (`chromecast:<id>`, ...), else the kind's lowercase name (`nidoo`, `snapcast`, `local`,
//! `stream`). Both are stable across restarts.

use serde::Serialize;

use super::protocol::FAVORITES_ID;
use crate::output::{OutputChoice, OutputKind, OutputOption, OutputView};
use crate::playlists::PlaylistSummary;

/// One menu entry.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Item {
    pub id: String,
    pub name: String,
}

/// A phone tab is gone by morning and the log engine makes no sound: neither is a target.
fn offered(kind: OutputKind) -> bool {
    !matches!(kind, OutputKind::Browser | OutputKind::Log)
}

fn kind_name(kind: OutputKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn option_id(o: &OutputOption) -> String {
    o.id.clone().unwrap_or_else(|| kind_name(o.kind))
}

/// The outputs a flow may pick, named as the output picker names them.
pub fn source_list(view: &OutputView) -> Vec<Item> {
    view.available
        .iter()
        .filter(|o| offered(o.kind))
        .map(|o| Item {
            id: option_id(o),
            name: o.name.clone(),
        })
        .collect()
}

/// The source id of a built output's id (`OutputInfo.id`, e.g. `nidoo:127.0.0.1:4953`),
/// `None` when no offered option stands for it.
pub fn id_of_output(view: &OutputView, output_id: &str) -> Option<String> {
    let prefix = output_id.split(':').next().unwrap_or(output_id);
    view.available
        .iter()
        .filter(|o| offered(o.kind))
        .find(|o| match &o.id {
            Some(id) => id == output_id,
            None => kind_name(o.kind) == prefix,
        })
        .map(option_id)
}

/// The id of the current output, in the same id space as [`source_list`].
pub fn current_source(view: &OutputView) -> Option<String> {
    id_of_output(view, &view.current.as_ref()?.info.id)
}

/// The choice to hand to `Outputs::set` for a source id.
pub fn choice_for(view: &OutputView, id: &str) -> Option<OutputChoice> {
    let option = view
        .available
        .iter()
        .filter(|o| offered(o.kind))
        .find(|o| option_id(o) == id)?;
    Some(OutputChoice {
        kind: option.kind,
        address: option.address.clone().or(option.default_address.clone()),
        name: None,
    })
}

/// Favorites first, then the playlists in the order given.
pub fn playlist_items(lists: &[PlaylistSummary]) -> Vec<Item> {
    std::iter::once(Item {
        id: FAVORITES_ID.into(),
        name: "Favorites".into(),
    })
    .chain(lists.iter().map(|p| Item {
        id: p.id.clone(),
        name: p.name.clone(),
    }))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{CurrentOutput, OutputInfo};

    fn opt(kind: OutputKind, name: &str, id: Option<&str>, addr: Option<&str>) -> OutputOption {
        OutputOption {
            kind,
            name: name.into(),
            needs_address: id.is_none() && kind != OutputKind::Stream,
            default_address: id.is_none().then(|| "127.0.0.1:4953".to_string()),
            note: None,
            id: id.map(Into::into),
            address: addr.map(Into::into),
        }
    }

    fn view(current: Option<(&str, OutputKind)>) -> OutputView {
        OutputView {
            current: current.map(|(id, kind)| CurrentOutput {
                info: OutputInfo {
                    id: id.into(),
                    kind,
                    name: "x".into(),
                    address: None,
                    hint: None,
                },
                reachable: None,
                speakers: false,
                approval: false,
                browser_player: false,
            }),
            startup_id: None,
            available: vec![
                opt(OutputKind::Browser, "This device", None, None),
                opt(OutputKind::Nidoo, "Nidoo", None, None),
                opt(OutputKind::Stream, "Stream URL", None, None),
                opt(OutputKind::Log, "Log", None, None),
                opt(
                    OutputKind::Chromecast,
                    "Kitchen TV",
                    Some("chromecast:abc"),
                    Some("abc"),
                ),
            ],
        }
    }

    fn summary(id: &str, name: &str) -> PlaylistSummary {
        PlaylistSummary {
            id: id.into(),
            name: name.into(),
            song_count: 1,
            artwork_urls: vec![],
            updated_at: "2026-11-20T00:00:00Z".into(),
        }
    }

    #[test]
    fn excludes_browser_and_log() {
        let ids: Vec<_> = source_list(&view(None)).into_iter().map(|i| i.id).collect();
        assert_eq!(ids, ["nidoo", "stream", "chromecast:abc"]);
    }

    #[test]
    fn discovered_device_uses_option_id() {
        let list = source_list(&view(None));
        assert_eq!(list[2].id, "chromecast:abc");
        assert_eq!(list[2].name, "Kitchen TV");
    }

    #[test]
    fn current_source_matches_list_id() {
        let v = view(Some(("nidoo:127.0.0.1:4953", OutputKind::Nidoo)));
        assert_eq!(current_source(&v).as_deref(), Some("nidoo"));
        let v = view(Some(("chromecast:abc", OutputKind::Chromecast)));
        assert_eq!(current_source(&v).as_deref(), Some("chromecast:abc"));
        let v = view(Some(("stream", OutputKind::Stream)));
        assert_eq!(current_source(&v).as_deref(), Some("stream"));
        // Never offered, so no source id.
        assert_eq!(current_source(&view(Some(("log", OutputKind::Log)))), None);
        assert_eq!(current_source(&view(None)), None);
    }

    #[test]
    fn choice_for_round_trips() {
        let v = view(None);
        for item in source_list(&v) {
            let choice = choice_for(&v, &item.id).expect(&item.id);
            let option = v
                .available
                .iter()
                .find(|o| option_id(o) == item.id)
                .unwrap();
            assert_eq!(choice.kind, option.kind);
        }
        let nidoo = choice_for(&v, "nidoo").unwrap();
        assert_eq!(nidoo.address.as_deref(), Some("127.0.0.1:4953"));
        let cast = choice_for(&v, "chromecast:abc").unwrap();
        assert_eq!(cast.address.as_deref(), Some("abc"));
        assert_eq!(choice_for(&v, "browser"), None);
        assert_eq!(choice_for(&v, "log"), None);
        assert_eq!(choice_for(&v, "nope"), None);
    }

    #[test]
    fn favorites_first() {
        let items = playlist_items(&[summary("p1", "Morning"), summary("p2", "Gym")]);
        let ids: Vec<_> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["favorites", "p1", "p2"]);
        assert_eq!(items[0].name, "Favorites");
        assert_eq!(playlist_items(&[]).len(), 1);
    }
}
