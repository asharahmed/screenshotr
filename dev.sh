#!/bin/sh
set -e

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

cargo tauri dev
