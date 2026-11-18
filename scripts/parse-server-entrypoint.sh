#!/bin/sh
set -eu

if [ "${1:-}" = "parse-server" ]; then
  # Seed missing Docling profiles from the image before accepting parse jobs.
  # This is a local copy into the persistent /data volume; it does not download.
  /usr/local/bin/sag docling install --data-dir /data --profile all
fi

exec /usr/local/bin/sag "$@"
