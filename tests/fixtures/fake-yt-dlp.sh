#!/bin/sh
# Test stand-in for yt-dlp. The last argument is the watch URL.
#   v=bad      -> prints a yt-dlp style error and exits 1
#   v=endless  -> endless WAV audio (never ends)
#   v=noclient -> fails when a player_client is given, plays with yt-dlp's default
#   otherwise  -> the 2 s sine fixture
dir=$(dirname "$0")
for url; do :; done
case "$*" in
  *player_client=*v=noclient) echo "ERROR: [youtube] noclient: Sign in to confirm you're not a bot" >&2; exit 1 ;;
esac
case "$url" in
  *v=bad) echo "ERROR: [youtube] bad: Video unavailable" >&2; exit 1 ;;
  *v=endless) exec ffmpeg -nostdin -loglevel error -f lavfi -i sine=frequency=440 -f wav - ;;
  *) exec cat "$dir/sine2s.wav" ;;
esac
