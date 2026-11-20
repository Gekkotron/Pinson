#!/bin/sh
# Test stand-in for yt-dlp. The video id comes from the last argument (the
# watch URL) or, with --load-info-json FILE, from FILE's name.
#   v=bad      -> prints a yt-dlp style error and exits 1
#   v=endless  -> endless WAV audio (never ends)
#   v=noclient -> fails when a player_client is given, plays with yt-dlp's default
#   otherwise  -> the 2 s sine fixture
# --load-info-json FILE: plays as for a normal id; FILE containing "expired"
#   fails like an expired stream URL (403).
# --write-info-json with -o infojson:TEMPLATE: writes TEMPLATE's <id>.info.json
#   (with "%(id)s" replaced), appends a line to <id>.runs next to it, then
#   streams as usual; with --skip-download it exits instead (after 1 s for
#   v=slow).
dir=$(dirname "$0")
prev=
infofile=
template=
write=0
skip=0
for a; do
  case "$prev" in
    --load-info-json) infofile=$a ;;
    -o) case "$a" in infojson:*) template=${a#infojson:} ;; esac ;;
  esac
  case "$a" in
    --write-info-json) write=1 ;;
    --skip-download) skip=1 ;;
  esac
  prev=$a
  url=$a
done
if [ -n "$infofile" ]; then
  if grep -q expired "$infofile"; then
    echo "ERROR: unable to download video data: HTTP Error 403: Forbidden" >&2
    exit 1
  fi
  id=$(basename "$infofile" .info.json)
  url="v=$id"
else
  case "$*" in
    *player_client=*v=noclient) echo "ERROR: [youtube] noclient: Sign in to confirm you're not a bot" >&2; exit 1 ;;
  esac
  id=${url##*v=}
fi
case "$url" in
  *v=bad) echo "ERROR: [youtube] bad: Video unavailable" >&2; exit 1 ;;
esac
if [ "$skip" = 1 ] && [ "$id" = slow ]; then sleep 1; fi
if [ "$write" = 1 ] && [ -n "$template" ]; then
  file=$(printf '%s' "$template" | sed "s/%(id)s/$id/").info.json
  echo '{}' > "$file"
  echo run >> "${file%.info.json}.runs"
fi
[ "$skip" = 1 ] && exit 0
case "$url" in
  *v=endless) exec ffmpeg -nostdin -loglevel error -f lavfi -i sine=frequency=440 -f wav - ;;
  *) exec cat "$dir/sine2s.wav" ;;
esac
