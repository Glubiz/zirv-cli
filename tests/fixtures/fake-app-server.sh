#!/bin/sh
# Stands in for `codex app-server`. Invoked as: fake-app-server.sh app-server [--help | --listen stdio:// [-c ...]]
# `fake-app-server.sh exec ...` is the `codex exec` fallback and is handed to fake-codex-agent.sh.
# Env:
#   FAKE_APPSERVER_TRANSCRIPT=<file>  '<' lines are printed, '>' lines consume one stdin line
#   FAKE_APPSERVER_MODE=ok|garbage|exit|hang   (default ok)
#   FAKE_APPSERVER_HELP=listen|nolisten        (default listen)
#   FAKE_APPSERVER_LOG=<file>                  append the argv of every app-server invocation
if [ "$1" = "exec" ]; then
  exec sh "$(dirname "$0")/fake-codex-agent.sh" "$@"
fi
[ -z "${FAKE_APPSERVER_LOG:-}" ] || printf '%s\n' "$*" >> "$FAKE_APPSERVER_LOG"
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
# Like `codex app-server --listen stdio://`: exit 0 once the client closes stdin.
cat >/dev/null
exit 0
