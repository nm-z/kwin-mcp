---
name: kwin-mcp
description: Use by default for every local GUI observation, interaction, screenshot, visual verification, desktop application, window, browser test, click, or typing task, even when the user does not mention KWin MCP. Route Codex's GUI work to its isolated KWin MCP copy without disturbing Nate's real session. Use Nate's session only when the requested result explicitly must occur there, such as opening a path for Nate or inspecting Nate's current windows.
---

# Route KWin MCP Work

Classify the required destination before acting. There are exactly two GUI targets:

1. Nate's real GUI session.
2. Codex's isolated KWin MCP session.

The shell is not a third GUI target. It can inspect or change host files, processes, and services, but it cannot prove a rendered result.

## Understand KWin MCP

Treat KWin MCP as Codex's disposable, near-1:1 local GUI copy of Nate's machine. It is not a cloud computer and it is not Nate's real GUI session.

KWin MCP carries the environment needed for real application behavior, including network access, Wi-Fi connectivity, cookies, tokens, sockets, and services. Assume those capabilities are available until the requested path proves otherwise inside the isolated session. Never claim Nate's real session is required merely because an application needs authentication, network access, a logged-in profile, a socket, or a service.

The viewer may appear as a window in Nate's session, but the applications shown inside it belong to the isolated session. Writes made by those applications land in the disposable overlay and do not persist to Nate's host files.

Use KWin MCP's own screenshot operation to inspect its session. Never run Spectacle against Nate's screen or use desktop-control commands to focus, raise, resize, move, or cover Nate's windows merely to obtain visual evidence. Nate may keep clicking, switching windows, and multitasking throughout the task. Treat that activity as normal and irrelevant to KWin MCP screenshots.

## Choose the destination

Use Codex's isolated KWin MCP session whenever Codex needs to:

- Launch and inspect a desktop application without disturbing Nate.
- Click, type, navigate, or reproduce a GUI workflow.
- Capture screenshots or verify rendered state.
- Test a GUI change against host files without changing or focusing Nate's current windows.
- Diagnose application startup, layout, dialogs, authentication, or visible behavior in a controlled session.

Do this even when Nate supplied a screenshot from his session, the application is already open there, or using Nate's window appears more convenient. The screenshot identifies the problem; it does not authorize taking over the source window.

Use Nate's real GUI session only when the requested outcome inherently belongs there:

- Nate directly asks to open a known path in a named desktop application.
- Nate explicitly asks about his currently visible windows, tabs, focus, or already-running application state.
- The action must remain visible or usable for Nate after Codex finishes.
- Nate explicitly asks Codex to manipulate a particular window already open in his session.

Do not infer any of these requirements from a screenshot, from an application being open, or from a request to fix and verify behavior. If the request can be completed and proven in the copied session, do it there.

Interpret "my session" as Nate's real GUI session. Interpret "your session" or "your KWin MCP session" as Codex's isolated session.

Use the shell for persistent host changes that do not require GUI interaction. After changing host state, use a fresh or uncontaminated KWin MCP session for visual verification unless Nate explicitly requested a change to a particular running window.

Prefer a purpose-built connector for semantic operations that depend on an existing account or application session. Use KWin MCP when rendered interaction or visual evidence is the required boundary.

## Operate the isolated session

1. Call `session_start` before every other KWin MCP operation. It is idempotent.
2. Launch the real production application and traverse the same visible workflow a user would.
3. Inspect the accessibility tree when structure is useful and use screenshots for rendered evidence.
4. Interact only inside the isolated session.
5. Capture the resulting isolated window with KWin MCP's screenshot operation.
6. Call `session_stop` when finished unless continued isolated state is explicitly required.

Do not replace a GUI path with HTTP requests, internal calls, mocks, or shell-only checks when the requested evidence is rendered behavior.

If the copied session cannot reach a required cookie, token, socket, service, or network resource, keep the failure inside KWin MCP, observe the exact missing boundary, and repair that boundary when it is in scope. Do not silently fall back to Nate's session and do not bother Nate merely to bypass a KWin MCP defect.

## Keep evidence scoped

- An isolated screenshot proves the rendered application path in Codex's near-1:1 KWin MCP copy without interrupting Nate.
- A host file change does not prove that Nate's running application adopted it.
- A process, successful launch command, `session_start`, or viewer window does not prove the requested GUI state.
- A KWin MCP overlay change is not a persistent fix.
- Authentication shown in the isolated application proves that isolated rendered session, not the state of Nate's currently open window.

## Routing examples

- "Fix this Obsidian setting and verify it visually": change the persistent host configuration through the authorized host path, then launch Obsidian and screenshot it in KWin MCP. Do not focus Nate's Obsidian merely for verification.
- "Here is a screenshot of the broken window, fix it": use the screenshot as evidence, reproduce and verify the result in KWin MCP, and leave Nate's source window alone.
- "Open this file in Kate for me": open the exact path in Kate in Nate's real GUI session.
- "Reproduce this dialog": use KWin MCP from launch through screenshot.
- "What is currently open on my desktop?": inspect Nate's real GUI session, not KWin MCP.
- "Check whether GitHub is logged in inside your Chrome": launch Chrome in KWin MCP and inspect the rendered page there.
