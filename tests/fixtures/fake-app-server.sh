#!/bin/sh
# Stands in for `codex app-server`. Invoked as: fake-app-server.sh app-server [--help | --listen stdio://]
# Env:
#   FAKE_APPSERVER_TRANSCRIPT=<file>  '<' lines are printed, '>' lines consume one stdin line
#   FAKE_APPSERVER_MODE=ok|garbage|exit|hang   (default ok)
#   FAKE_APPSERVER_HELP=listen|nolisten        (default listen)
if [ "$2" = "--help" ]; then
  echo "Usage: codex app-server [OPTIONS]"
  [ "${FAKE_APPSERVER_HELP:-listen}" = "listen" ] && echo "      --listen <URL>"
  exit 0
fi
case "${FAKE_APPSERVER_MODE:-ok}" in
  exit) exit 3 ;;
  hang) exec sleep 30 ;;
  garbage) read -r _; echo "this is not json"; exec sleep 30 ;;
esac
while IFS= read -r step <&3; do
  case "$step" in
    '<'*) printf '%s\n' "${step#<}" ;;
    '>'*) read -r _ || exit 0 ;;
  esac
done 3<"$FAKE_APPSERVER_TRANSCRIPT"
exec sleep 30
