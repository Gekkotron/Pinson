//! Turns a parsed MQTT command into player actions.

use crate::api::{Services, play_something};
use crate::mqtt::protocol::Command;

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
    })
}

fn echo_if(acted: bool) -> Outcome {
    if acted {
        Outcome::Echo
    } else {
        Outcome::Nothing
    }
}
