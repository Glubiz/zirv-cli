## Memory
- Key: mail-memory-header-block-forgery
- Written-by: claude
- Written: 1790668315
- Verified: 1790668315
- Source: explicit

mail and memory parse_markdown must end the header block at the first blank line and never re-inspect body headings, or a body bullet can re-address the message or forge sender/key/source (#326).
