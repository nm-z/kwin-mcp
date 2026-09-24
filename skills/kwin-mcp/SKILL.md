---
name: kwin-mcp
description: Use a configured KWin MCP server for local desktop interaction and visual verification without controlling the user's current windows. Use the host desktop when the requested result must appear there.
---

# Use the correct desktop

KWin MCP starts a separate KDE desktop on the same host. Its windows and input are separate from the user's current desktop. Applications in that session can still reach host services that the server forwards, and actions against websites or external accounts are real.

Use the KWin MCP session for GUI workflows that you can complete and verify in that separate desktop. This includes launching applications, navigating websites, testing dialogs, and capturing rendered results. A screenshot of the user's desktop is evidence for a task; it does not by itself require control of the window that produced it.

Use the user's current desktop when the requested result must remain there. Examples include opening a file in a named application for the user, inspecting their currently open windows, or changing a specific window they identified. Follow the user's chosen destination if they specify one.

Prefer a purpose-built connector when the task needs application data or operations rather than rendered interaction. Do not infer that KWin MCP lacks a cookie, network route, socket, or service without checking the requested workflow in its session.

## Operate the isolated session

1. Call `session_start` before other KWin MCP tools. It is idempotent and returns information about an already running session.
2. Launch the application and perform the requested visible workflow through KWin MCP tools.
3. Use `accessibility_tree` or `find_ui_elements` for structure, and `screenshot` to verify rendered state. KWin MCP mouse and screenshot coordinates are relative to the active window.
4. Distinguish an isolated result from a persistent host change. Application writes under the isolated `$HOME` go to the session overlay; make authorized persistent file changes through an appropriate host path.

`session_stop` tears down the whole isolated session and its applications. Check ownership and the user's requested continued state before stopping it. Do not stop a session that another task owns.

Use KWin MCP screenshots for evidence from its session. A successful process launch or `session_start` response does not prove that the application rendered or accepted the requested interaction. Do not manipulate the user's current desktop merely to capture evidence from the isolated session.
