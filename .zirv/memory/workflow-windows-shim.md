## Memory
- Key: workflow-windows-shim
- Written-by: claude
- Written: 1790668295
- Verified: 1790668295
- Source: explicit

An npm .cmd adapter on Windows launches through cmd.exe /c, placing a launcher prefix before exec in argv, so argv-shape checks must strip that prefix and assert the invariant, not a fixed argv shape.
