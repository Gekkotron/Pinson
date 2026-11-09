#!/usr/bin/env bash
# Update a Docker install: pull the latest code, rebuild the image and
# restart Pinson. Restarting stops the song that is playing.
#
# Usage: ./update.sh [--no-pull]
#   --no-pull  rebuild and restart from the code already checked out
#
# Runs from the repo root whatever the current directory, so compose.yml and
# ./data are the ones next to this checkout.
set -euo pipefail

# Everything lives in main so bash has read the whole script before
# `git pull` rewrites this file.
main() {
  local pull=1
  for arg in "$@"; do
    case "$arg" in
      --no-pull) pull=0 ;;
      -h|--help)
        sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
      *)
        echo "unknown option: $arg" >&2
        echo "usage: ./update.sh [--no-pull]" >&2
        exit 2
        ;;
    esac
  done

  cd "$(dirname "$0")"

  if [ ! -f data/pinson.db ] && [ ! -f data/.env ]; then
    echo "warning: data/ has no pinson.db or .env; Pinson will start empty." >&2
  fi

  if [ "$pull" = 1 ]; then
    local before
    before="$(git rev-parse --short HEAD)"
    git pull --ff-only
    echo "code: $before -> $(git rev-parse --short HEAD)"
  fi

  docker compose up -d --build
  docker compose ps pinson
  echo
  echo "Last log lines (follow with: docker compose logs -f pinson):"
  sleep 2
  docker compose logs --tail 20 pinson
}

main "$@"
