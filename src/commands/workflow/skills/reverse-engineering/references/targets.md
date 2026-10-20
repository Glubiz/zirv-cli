# Route by target

Pick the row for the target class, run its first tool, then follow what it
returns. Always hash first: `shasum -a 256 <target>` and `file <target>`.
For target-free inspections the CLI verb is the tool name with hyphens
(`inspect_wasm_artifact` is `rea inspect-wasm-artifact`). Native analysis
is one-shot: `rea analyze|search|function|decompile|xrefs|trace <abs path>
--provider <id>`; there is no `open-binary` verb. Pass absolute paths, and
add `--json`.

| Target | First REA tool | Local fallback | What the fallback cannot establish |
|---|---|---|---|
| Native exe or library (Mach-O, ELF, PE) | `open_binary`, then `analyze_function`, xrefs, decompile; `close_binary` at the end | `otool -hl/-L`, `nm`, `objdump -d`, `readelf`, `codesign -dvv` | No decompilation, no reliable cross-references or call graph; needs Ghidra, IDA or Hopper |
| Binary layout (sections, segments, imports) | `inspect_binary_layout` | `otool -l`, `readelf -a`, `objdump -x` | Raw headers only, no interpretation of what a section is used for |
| ZIP, APK, IPA, MSIX, DMG member inventory | `open_binary` + `inspect_artifact` | `unzip -l`, `hdiutil imageinfo` | Listing only; no per-member analysis or content hashes unless you add them |
| APK code and manifest | `inspect_android_package` | `apktool d -o <newdir> app.apk`, `jadx -d <newdir> app.apk` (if installed), `aapt dump badging` | Decompiled Java is a reconstruction; native libs and obfuscated code stay opaque |
| Managed PE / .NET assembly | `inspect_managed_artifact` | `ilspycmd` (if installed) | Without it, `strings` and `monodis` give names only, no logic |
| ASAR or extracted JavaScript app | `analyze_javascript_application` | `asar list app.asar`, `asar extract app.asar <newdir>`, `grep`, `rg` over the bundle | Text search finds strings, not module graph, routes or data flow; minified code is not read |
| Firmware image | `inspect_firmware_regions`, then `extract_firmware` into a new absolute output dir you choose | `binwalk`, `file`, `strings` | Entropy and signatures are hints; extraction can miss or misidentify regions |
| WebAssembly | `inspect_wasm_artifact` | `wasm-objdump -x`, `wasm2wat` | Text format shows code, not intent; imports and exports are the reliable start |
| EVM bytecode | `inspect_evm_interface` | none dependable | Selector guesses from bytes are inference |
| Go binary | `inspect_go_binary` | `go version -m`, `strings`, `nm` | Build info is observed; function purpose is not |
| Rust binary | `inspect_rust_binary` | `strings` for panic paths and crate names, `nm` | Crate names are hints, not proof of linked code in use |
| Unity IL2CPP | `inspect_il2cpp_metadata` (the `global-metadata.dat`) | `strings` | Names only; no method bodies |
| SQLite database | `inspect_sqlite_database` | `sqlite3 -readonly file '.schema'` | Open read-only; a live WAL can change on open |
| Core file | `inspect_recorded_crash` (Linux x86-64 ELF cores only) | `lldb -c core exe` or `gdb exe core`, batch mode; use this for any other core | Needs the exact matching binary and symbols |
| HAR, SAZ, qlog | `inspect_web_network_capture` | `jq` over HAR | Absence of a request in a capture is not absence of the behaviour |
| pcap / pcapng | `inspect_packet_capture`, `inspect_pcap_capture` | `tshark -r file -q -z ...` | Encrypted payloads stay opaque without keys |
| Signal recording | `decode_signal_recording` | none dependable | Needs the decoder and parameters; guesses are inference |
| PE resources | `inspect_pe_resources` | GNU strings (`gstrings -el` on macOS, `strings -el` on Linux) or `7z l` | Resource types and versions need a parser |
| Electron / web page, running | `list_electron_targets`, `inspect_electron_fuses` | browser devtools if the user opens them | Runtime observation: see `runtime-and-engine.md` |

## Rules

- Open an archive with `inspect_artifact` before extracting; extract only
  into a new directory you create outside the target.
- Disassembly (`otool -tV`, `objdump -d`) is a view of bytes: say
  "disassembly shows", never "the program does".
- A local fallback result still gets a ledger record with the exact
  command, tool version and output file.
- If the class is not listed, say so, choose the closest read-only tool,
  and record the gap as an Unknown.
