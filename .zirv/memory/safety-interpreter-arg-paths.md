## Memory
- Key: safety-interpreter-arg-paths
- Written-by: claude
- Written: 1790596361
- Verified: 1790596361
- Source: explicit

Word-by-word path checks miss protected file names embedded inside interpreter arguments (a path inside a python -c or node -e string).
