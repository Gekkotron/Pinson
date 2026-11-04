#!/bin/sh
# Test stand-in for yt-dlp. The last argument is the watch URL.
#   v=bad      -> prints a yt-dlp style error and exits 1
#   v=endless  -> endless WAV audio (never ends)
#   otherwise  -> the 2 s sine fixture
dir=$(dirname "$0")
for url; do :; done
case "$url" in
  *v=bad) echo "ERROR: [youtube] bad: Video unavailable" >&2; exit 1 ;;
  *v=endless) exec ffmpeg -nostdin -loglevel error -f lavfi -i sine=frequency=440 -f wav - ;;
  *) exec cat "$dir/sine2s.wav" ;;
esac
