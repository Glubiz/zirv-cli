@echo off
rem Fixture for the native MCP client's stdio transport (issue #483 review
rem finding 1): the Windows sibling of mcp-hang-server.sh. See that file's
rem header for the protocol.
rem `set /p` keeps one line per read and drops the rest of the pipe chunk, so
rem each line is acknowledged in $MCP_HANG_READY once it has been consumed; the
rem test waits for that before it sends the next line (#635).
setlocal enabledelayedexpansion
:loop
set "line="
set /p "line="
if defined line (
    if defined MCP_HANG_READY echo ready>"%MCP_HANG_READY%"
    echo(!line!| findstr /C:"notifications/cancelled" >nul
    if not errorlevel 1 if defined MCP_HANG_MARKER echo cancelled>>"%MCP_HANG_MARKER%"
)
goto loop
