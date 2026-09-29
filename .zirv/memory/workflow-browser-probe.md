## Memory
- Key: workflow-browser-probe
- Written-by: claude
- Written: 1790668295
- Verified: 1790668295
- Source: explicit

Installed Chrome or Edge can take many seconds (up to most of a minute) to exit a headless --version probe, so workflow capability admission uses launch-free browser_present discovery and reports the browser as unverified (~15s wall time seen per workflow start before the change).
