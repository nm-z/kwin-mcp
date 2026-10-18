# Gemini rich-text delivery receipt (#378)

This opt-in real-site check uses the production stdio server and an owned
isolated desktop. It uses the signed-in Chrome profile copied by `session_start`;
it does not control an existing host browser window. KDE Wayland, Google Chrome,
and a Gemini account exposing Flash High are required. It performs no model
request: the synthetic prompt is typed once and never submitted.

Build the server, then run from the repository root:

```bash
cargo build --bin kwin-mcp --jobs 2
KWIN_MCP_E2E_SERVER="$PWD/target/debug/kwin-mcp" \
KWIN_MCP_TEST_ARTIFACT_DIR="$PWD/target/gemini-input-delivery" \
ruby tests/gemini_input_delivery_e2e.rb
```

The check selects Flash High, enables Temporary chat, focuses `Enter a prompt
for Gemini`, selects its contents, and types the original 33-character probe
from #378. It requires all of the following:

- The compositor acknowledges all characters, submission completes, and
  `retry_safe` remains false.
- A separate `find_ui_elements` call finds the complete prompt before any
  submission. The check also saves a crop of the prompt field.
- The receipt reports `verified`, method `atspi_rich_text`, expected length 33,
  and focused-node observed length 1. This specifically exercises rich-text
  descendant verification rather than accepting any successful key submission.

The probe inspects the independent page evidence even if `keyboard_type`
returns an error. It then fails on that error; it never resends the text.
`ensure` stops the owned session and closes the server transport on either
success or failure. RPC evidence, the receipt, the matching static text, and
`prompt.png` are saved under the artifact directory. Screenshot image data is
omitted from the RPC log.

The original `a34524a` server should fail with `pending_or_mismatched`, expected
33, observed 1, while the independent static text still matches. The receipt
implementation from #310 is expected to verify the changed descendant text.
This check supplies Gemini-specific coverage for that existing fix. Changes to
the website's English control labels, available modes, or accessibility shape
can cause the check to fail and require investigation.

## Observed comparison (2026-10-15)

The same probe ran against both builds on engi with KDE Wayland and the copied
signed-in Chrome profile, Flash High, Temporary chat, and a 1280×900 display.

| Server commit | Result | Receipt method | Expected / observed | Typing call |
| --- | --- | --- | --- | --- |
| `a34524a` | `pending_or_mismatched`, error | `atspi_focused_text` | 33 / 1 | 14.088 s |
| `3dffc29` (merged #310) | `verified` | `atspi_rich_text` | 33 / 1 | 3.882 s |

Both runs independently found the complete synthetic prompt in the composer and
saved a matching field crop. Both reported completed submission and compositor
acknowledgement with `retry_safe=false`; both removed their owned session
workdirs at stop. Neither sent Return or requested a model response.
