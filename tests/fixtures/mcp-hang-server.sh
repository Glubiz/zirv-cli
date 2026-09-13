#!/bin/sh
# Fixture for the native MCP client's stdio transport (issue #483 review
# finding 1): never answers any request, so a caller blocked inside
# `transport.request` stays blocked until it gives up or is cancelled. If the
# caller follows up a cancellation with a `notifications/cancelled`
# notification, appends a marker line to $MCP_HANG_MARKER so the test can
# confirm it reached the (still-alive) server. The `.cmd` sibling does the
# same thing on Windows.
while IFS= read -r line; do
    case "$line" in
        *notifications/cancelled*)
            if [ -n "$MCP_HANG_MARKER" ]; then
                printf 'cancelled\n' >> "$MCP_HANG_MARKER"
            fi
            ;;
        *)
            ;;
    esac
done
