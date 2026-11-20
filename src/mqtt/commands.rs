//! Turns a parsed MQTT command into player actions.

use rand::seq::SliceRandom;

use crate::api::{Services, play_something};
use crate::mqtt::protocol::{Command, FAVORITES_ID, PlayPlaylist};
use crate::mqtt::sources::{choice_for, current_source, id_of_output};
use crate::output::OutputKind;
use crate::song::Song;

/// What the publisher should do after a command.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Done; the normal debounced state publish covers it.
    Done,
    /// The player acted on a skip or a new song: force-publish now.
    Echo,
    /// Nothing happened (or it failed and was logged).
    Nothing,
}

/// Runs one command. Never touches the Autoplay switch and never fails:
/// errors are logged and answered `Nothing`.
pub async fn dispatch(app: &Services, cmd: Command) -> Outcome {
    match run(app, cmd).await {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!("mqtt command failed: {e:#}");
            Outcome::Nothing
        }
    }
}

async fn run(app: &Services, cmd: Command) -> anyhow::Result<Outcome> {
    Ok(match cmd {
        Command::Play => {
            if app.player.state().current.is_some() {
                app.player.resume().await?;
                Outcome::Done
            } else if play_something(app).await? {
                Outcome::Done
            } else {
                tracing::warn!("mqtt play: nothing to play (no favourites, no Home mix)");
                Outcome::Nothing
            }
        }
        Command::Pause => {
            app.player.pause().await?;
            Outcome::Done
        }
        Command::Next => echo_if(app.player.skip(None).await?),
        Command::Previous => echo_if(app.player.previous(None).await?),
        Command::PlayMedia(query) => {
            let Some(top) = app.searcher.search(&query).await?.into_iter().next() else {
                tracing::warn!("mqtt play_media: no result for {query:?}");
                return Ok(Outcome::Nothing);
            };
            app.player.play_all(vec![top]).await?;
            Outcome::Echo
        }
        Command::PlayPlaylist(cmd) => play_playlist(app, cmd).await?,
    })
}

fn echo_if(acted: bool) -> Outcome {
    if acted {
        Outcome::Echo
    } else {
        Outcome::Nothing
    }
}

/// The wake-up start (spec "Pinson's behaviour"): output, songs, sleep timer, fade-in.
/// The output switch and the sleep cancel come first because the player ends an armed
/// fade-in on an output switch.
async fn play_playlist(app: &Services, cmd: PlayPlaylist) -> anyhow::Result<Outcome> {
    switch_output(app, cmd.source.as_deref()).await;
    let mut songs = songs_for(app, &cmd.playlist);
    if cmd.shuffle {
        songs.shuffle(&mut rand::rng());
    }
    app.player.set_sleep(None).await?;
    if songs.is_empty() {
        tracing::warn!("mqtt play_playlist: no favourites either, playing something");
        return Ok(echo_if(play_something(app).await?));
    }
    let played = app.player.play_all_with_fade_in(songs, cmd.fade_in).await?;
    Ok(echo_if(played > 0))
}

/// Moves to `source` when asked, then off a browser tab or the log output (a phone tab is
/// gone by morning) onto the startup output when there is one. Failures keep the output.
async fn switch_output(app: &Services, source: Option<&str>) {
    let view = app.outputs.view();
    if let Some(id) = source
        && current_source(&view).as_deref() != Some(id)
    {
        match choice_for(&view, id) {
            Some(choice) => {
                if let Err(e) = app.outputs.set(choice).await {
                    tracing::warn!("mqtt play_playlist: could not switch to {id:?}: {e:?}");
                }
            }
            None => tracing::warn!("mqtt play_playlist: unknown source {id:?}, keeping the output"),
        }
    }
    let view = app.outputs.view();
    let silent = view
        .current
        .as_ref()
        .is_some_and(|c| matches!(c.info.kind, OutputKind::Browser | OutputKind::Log));
    if !silent {
        return;
    }
    let startup = view
        .startup_id
        .as_deref()
        .and_then(|id| id_of_output(&view, id))
        .and_then(|id| choice_for(&view, &id));
    match startup {
        Some(choice) => {
            if let Err(e) = app.outputs.set(choice).await {
                tracing::warn!(
                    "mqtt play_playlist: could not go back to the startup output: {e:?}"
                );
            }
        }
        None => tracing::warn!(
            "mqtt play_playlist: the output is a browser tab or log, no startup output to use"
        ),
    }
}

/// The playlist's songs; the Favorites for `favorites`, or when the playlist is gone or empty.
fn songs_for(app: &Services, id: &str) -> Vec<Song> {
    if id != FAVORITES_ID {
        match app.playlists.get(id) {
            Some(p) if !p.songs.is_empty() => return p.songs,
            Some(_) => {
                tracing::warn!("mqtt play_playlist: playlist {id:?} is empty, using Favorites")
            }
            None => tracing::warn!("mqtt play_playlist: unknown playlist {id:?}, using Favorites"),
        }
    }
    app.favorites.list()
}
