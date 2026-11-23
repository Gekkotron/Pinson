#!/bin/sh
# Test stand-in for `yt-dlp -o <dir>/<id>.%(ext)s <url>`: writes the file as
# <id>.webm. Like fake-yt-dlp.sh: v=bad fails; v=noclient fails when a
# player_client is given.
prev=
out=
for a; do
  [ "$prev" = -o ] && out=$a
  prev=$a
  url=$a
done
case "$url" in
  *v=bad) echo "ERROR: [youtube] bad: Video unavailable" >&2; exit 1 ;;
esac
case "$*" in
  *player_client=*v=noclient) echo "ERROR: [youtube] noclient: Sign in to confirm you're not a bot" >&2; exit 1 ;;
esac
file=$(printf '%s' "$out" | sed 's/%(ext)s/webm/')
printf 'audio' > "$file"
