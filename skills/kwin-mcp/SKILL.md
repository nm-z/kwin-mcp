---
name: kwin-mcp
description: Control desktop apps, browsers, windows, clicks, typing, screenshots, and visual verification through the agent's black-box carbon-copy live desktop. Use it by default for GUI/CUA work because it carries the user's copied session capabilities without moving or focusing windows on the user's active desktop. Do not invoke it when the user directly asks to open a known path in a named desktop application on their active desktop, inspect their current windows, or produce another result that must remain on that desktop.
---

# Control GUI through KWin MCP

## Use the correct mental model

KWin MCP is a black-box carbon-copy live desktop on the user's machine. It starts with the user environment and session capabilities needed to operate the same applications and websites the user can operate, including Chrome profile state, cookies, logged-in accounts, network access, Wi-Fi connectivity, tokens, sockets, and services.

It is neither the user's active desktop nor an unrelated blank computer. Its windows and input run on a separate display, so clicking, typing, focusing, moving, or closing a window there does not disturb the user's active desktop. Do not treat that display separation as missing access, authentication, or capability.

If the user can complete an authorized task through a website or desktop application, perform that full path in KWin MCP. Do not ask the user to click, type, navigate, sign in, or operate an application merely because the path needs GUI interaction or existing session state. If the requested path proves that a specific capability is absent, diagnose that observed boundary in KWin MCP.

Actions against external services are real. A message, form submission, account change, purchase, upload, or other external write made through KWin MCP affects the live service and follows the same authorization boundaries as any other external action.

Local writes made by applications under the copied `$HOME` land in KWin MCP's disposable overlay and do not persist to the user's host files. Make authorized persistent host-file changes through the shell, then use KWin MCP to verify rendered behavior.

## Route GUI work

Use KWin MCP by default to launch and inspect desktop applications, browse websites, click, type, reproduce GUI behavior, capture screenshots, and verify rendered results. A screenshot from the user or an application already open on their desktop is evidence, not authorization to control that source window.

Use the user's active desktop only when the requested result inherently belongs there, including these cases:

- The user directly asks to open a known path in a named desktop application.
- The user asks about or asks the agent to manipulate a window, tab, focus state, or application already open on their active desktop.
- The result must remain visible or usable on the user's active desktop after the agent finishes.

"My session" means the user's active desktop. "KWin MCP," "your session," and "your KWin MCP session" mean the carbon-copy live desktop.

## Operate and prove the path

1. Call `session_start` before every other KWin MCP operation. It is idempotent.
2. Launch the production application and traverse the same visible workflow the user would use.
3. Start with a screenshot and visual input. Use `find_ui_elements` for a specific named control, and a filtered accessibility tree only when structure helps. If the tree is empty or larger than the visual task warrants, continue with screenshots, keyboard, and mouse input.
4. Interact inside the carbon-copy desktop and verify the resulting visible state there.
5. Call `session_stop` when finished unless continued state is required.

Do not replace a requested GUI path with HTTP calls, internal calls, mocks, or shell-only checks. A process, launch result, `session_start`, or viewer window does not prove the requested GUI outcome.

The viewer can appear as a window on the user's active desktop, but the applications inside it still belong to the carbon-copy desktop. Use KWin MCP's screenshot operation for evidence. Do not use Spectacle or desktop-control commands against the user's active desktop merely to inspect, focus, raise, resize, move, or cover their windows.
