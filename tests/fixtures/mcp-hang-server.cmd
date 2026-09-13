@echo off
rem Fixture for the native MCP client's stdio transport (issue #483 review
rem finding 1): the Windows sibling of mcp-hang-server.sh. See that file's
rem header for the protocol.
setlocal enabledelayedexpansion
:loop
set "line="
set /p "line="
if defined line (
    echo(!line!| findstr /C:"notifications/cancelled" >nul
    if not errorlevel 1 if defined MCP_HANG_MARKER echo cancelled>>"%MCP_HANG_MARKER%"
)
goto loop
