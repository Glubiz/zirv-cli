## Memory
- Key: supervise-eperm-live
- Written-by: claude
- Written: 1790668231
- Verified: 1790668231
- Source: explicit

A sandbox can refuse a kill with EPERM while the process remains alive and holds its writer permit; report it as refused, not terminated (#403).
