## Memory
- Key: dash-kitty-query
- Written-by: claude
- Written: 1790595371
- Verified: 1790595371
- Source: explicit

Keyboard enhancement probing shares stdin with event reads, so probing after the dashboard input loop starts can race it for reply bytes.
