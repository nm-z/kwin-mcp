# Typed punctuation under a scheduler pause

Issue #472 reported extra punctuation despite one submitted character per key.
The producer previously awaited a timer while a printable key was pressed.
The change queues its press and release before flushing and waits only between
complete keystrokes. Explicit held-key APIs retain their existing behavior.

## Reproduction on engi

Base: `05b937d`. Run the committed production-stdio probe with an already built
server and an empty evidence directory:

```sh
ruby tests/typed_key_batch_e2e.rb SERVER EVIDENCE baseline
ruby tests/typed_key_batch_e2e.rb SERVER EVIDENCE candidate
```

Each run creates a private KWin desktop and native Qt input field. The probe
pauses only its own MCP server PID for two seconds after the first logged
hyphen press. KWin and the application continue processing. It independently
copies the field through the desktop clipboard, saves a real screenshot, and
then stops the owned desktop. The baseline gains extra hyphens; the candidate
retains the exact requested text. The candidate also checks repeated hyphens,
shift transitions, and punctuation without an injected pause.

Raw PNGs, expected/observed text, and response metadata are retained beside this
file. Both modes passed their stated assertions.

Queueing both transitions removes the awaited key-up interval; it does not
claim an atomic Unix-stream write or a bound on operating-system scheduling.
