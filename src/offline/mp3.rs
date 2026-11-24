use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use id3::frame::{
    Lyrics as LyricsFrame, Picture, PictureType, SynchronisedLyrics, SynchronisedLyricsType,
    TimestampFormat, UniqueFileIdentifier,
};
use id3::{Tag, TagLike, Version};
use tokio::process::Command;

use super::{Downloader, valid_video_id};
use crate::artwork::{Artwork, ArtworkProxy};
use crate::lyrics::{Lyrics, LyricsQuery, LyricsService};
use crate::song::Song;
use crate::source::YtDlpFfmpegSource;

const VIDEO_ID_OWNER: &str = "https://music.youtube.com/";
const COVER_SIZE: u32 = 600;
const LYRICS_TIMEOUT: Duration = Duration::from_secs(15);

/// Converts offline audio and embeds the metadata in the MP3 itself.
pub struct Mp3Downloader {
    source: Arc<YtDlpFfmpegSource>,
    artwork: Arc<ArtworkProxy>,
    lyrics: Arc<LyricsService>,
}

impl Mp3Downloader {
    pub fn new(
        source: Arc<YtDlpFfmpegSource>,
        artwork: Arc<ArtworkProxy>,
        lyrics: Arc<LyricsService>,
    ) -> Self {
        Self {
            source,
            artwork,
            lyrics,
        }
    }

    async fn encode(&self, input: &Path, output: &Path) -> anyhow::Result<()> {
        let result = Command::new(&self.source.ffmpeg)
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(input)
            .args([
                "-map",
                "0:a:0",
                "-map_metadata",
                "-1",
                "-c:a",
                "libmp3lame",
                "-q:a",
                "2",
            ])
            .arg(output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .context("encoding offline MP3 with ffmpeg")?;
        if !result.status.success() {
            bail!(
                "ffmpeg failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            );
        }
        Ok(())
    }

    async fn audio(
        &self,
        song: &Song,
        legacy: Option<&Path>,
        tmp: &Path,
        output: &Path,
    ) -> anyhow::Result<()> {
        if let Some(input) = legacy {
            match self.encode(input, output).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("offline: {}: local conversion failed: {e:#}", song.video_id)
                }
            }
        }
        let input = self
            .source
            .download(&song.video_id, &tmp.join("source"))
            .await?;
        self.encode(&input, output).await
    }

    async fn cover(&self, song: &Song) -> Option<Artwork> {
        if let Some(url) = song.artwork_url.as_deref()
            && let Ok(cover) = self.artwork.fetch_resized(url, COVER_SIZE).await
        {
            return Some(cover);
        }
        let thumbnail = format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", song.video_id);
        self.artwork
            .fetch_resized(&thumbnail, COVER_SIZE)
            .await
            .ok()
    }

    async fn lyrics(&self, song: &Song) -> Option<Lyrics> {
        let query = LyricsQuery {
            video_id: song.video_id.clone(),
            title: song.title.clone(),
            artists: song.artists.clone(),
            album: song.album.clone(),
            duration_secs: song.duration_secs,
        };
        match tokio::time::timeout(LYRICS_TIMEOUT, self.lyrics.lyrics(&query)).await {
            Ok(Ok(lyrics)) => lyrics,
            Ok(Err(e)) => {
                tracing::debug!("offline: {}: lyrics unavailable: {e:#}", song.video_id);
                None
            }
            Err(_) => None,
        }
    }
}

#[async_trait]
impl Downloader for Mp3Downloader {
    async fn download(
        &self,
        song: &Song,
        legacy: Option<&Path>,
        tmp_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        tokio::fs::create_dir_all(tmp_dir).await?;
        let output = tmp_dir.join("audio.mp3");
        let (_, cover, lyrics) = tokio::try_join!(
            self.audio(song, legacy, tmp_dir, &output),
            async { Ok(self.cover(song).await) },
            async { Ok(self.lyrics(song).await) },
        )?;
        let tag = track_tag(song, cover, lyrics);
        // Open before handing off the work so cancellation cannot retag a later download.
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&output)
            .await?;
        let file = file.into_std().await;
        tokio::task::spawn_blocking(move || tag.write_to_file(file, Version::Id3v23)).await??;
        Ok(output)
    }
}

fn track_tag(song: &Song, cover: Option<Artwork>, lyrics: Option<Lyrics>) -> Tag {
    let mut tag = Tag::new();
    tag.set_artist(&song.artists);
    tag.set_title(&song.title);
    if let Some(album) = song
        .album
        .as_deref()
        .filter(|album| !album.trim().is_empty())
    {
        tag.set_album(album);
    }
    tag.add_frame(UniqueFileIdentifier {
        owner_identifier: VIDEO_ID_OWNER.into(),
        identifier: song.video_id.as_bytes().to_vec(),
    });
    if let Some(artwork) = cover {
        tag.add_frame(Picture {
            mime_type: artwork.content_type,
            picture_type: PictureType::CoverFront,
            description: String::new(),
            data: artwork.bytes.to_vec(),
        });
    }
    if let Some(lyrics) = lyrics {
        let plain = lyrics.plain.unwrap_or_else(|| {
            lyrics
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        });
        if !plain.trim().is_empty() {
            tag.add_frame(LyricsFrame {
                lang: "und".into(),
                description: String::new(),
                text: plain,
            });
        }
        if lyrics.synced && !lyrics.lines.is_empty() {
            tag.add_frame(SynchronisedLyrics {
                lang: "und".into(),
                timestamp_format: TimestampFormat::Ms,
                content_type: SynchronisedLyricsType::Lyrics,
                description: String::new(),
                content: lyrics
                    .lines
                    .into_iter()
                    .filter_map(|line| {
                        Some((
                            u32::try_from(line.time_ms).ok()?,
                            format!("{}\n", line.text),
                        ))
                    })
                    .collect(),
            });
        }
    }
    tag
}

pub(super) fn video_id(path: &Path) -> Option<String> {
    let tag = Tag::read_from_path(path).ok()?;
    let frame = tag
        .unique_file_identifiers()
        .find(|frame| frame.owner_identifier == VIDEO_ID_OWNER)?;
    let id = std::str::from_utf8(&frame.identifier).ok()?;
    valid_video_id(id).then(|| id.to_owned())
}

/// Keep user-facing names readable and valid across common filesystems.
fn name_part(value: &str, fallback: &str) -> String {
    let value: String = value
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Leave room for the separator, extension and collision suffix.
    let value = value[..value.floor_char_boundary(90)].trim_end_matches([' ', '.']);
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

/// Link the completed file into the cache without replacing a track that has
/// the same name. The staging file is removed by the offline worker.
pub(super) fn publish(dir: &Path, song: &Song, file: &Path) -> anyhow::Result<PathBuf> {
    let artist = name_part(&song.artists, "Unknown Artist");
    let title = name_part(&song.title, "Untitled");
    let stem = format!("{artist} - {title}");
    let mut dest = dir.join(format!("{stem}.mp3"));
    let mut attempt = 1;
    loop {
        match std::fs::hard_link(file, &dest) {
            Ok(()) => return Ok(dest),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let id = &song.video_id;
                dest = if attempt == 1 {
                    dir.join(format!("{stem} [{id}].mp3"))
                } else {
                    dir.join(format!("{stem} [{id}] ({attempt}).mp3"))
                };
                attempt += 1;
            }
            Err(e) => return Err(e).with_context(|| format!("publishing {}", dest.display())),
        }
    }
}
