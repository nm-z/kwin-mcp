# Screenshot metadata regression evidence

Inline screenshots keep their image and JSON metadata text blocks by default.
Clients that support images alongside structured results can request
`{"inline": true, "structured_metadata": true}` to additionally receive the same
metadata in `structuredContent`. File captures always include structured metadata.
This opt-in addresses the review concern that Claude Code can hide `content[]`
when a result also has `structuredContent`.

## Capture comparison on engi, 2026-10-20

These captures use a synthetic KDialog in a private 1920×1080 desktop
on KWin 6.7.5, with a private HOME and no host browser profiles.

| Default branch (`05b937d`), default inline | Fixed build (`e5f700d`), metadata opted in |
| --- | --- |
| ![Inline capture before the fix](before.png) | ![Inline capture with metadata](after.png) |

The independently captured PNGs are byte identical (SHA-256
`c2c8e564f536796935a8bb2db95159a36cb3d41a3f7ce04b80848bad49d733cc`).
The change concerns the response envelope; it does not change capture pixels.

## Current regression

```bash
ruby tests/screenshot_metadata_e2e.rb /path/to/kwin-mcp /path/to/evidence
```

The script creates a private desktop through the production stdio entrypoint,
launches a synthetic KDialog, and checks its window independently with
`window_list`. It covers nine captures: file; default inline; explicitly disabled
structured metadata; opted-in full inline; cropped inline; cursor inline; failed
file saves with and without structured metadata; and a recovered file save. It
also checks that the optional Boolean is advertised in the screenshot schema.

Each inline response retains its text, JSON text, and PNG image blocks. The script
compares opted-in structured metadata with the JSON text, verifies PNG dimensions
against metadata, and compares each saved inline PNG byte for byte with its image
block. Default and explicitly disabled inline results must omit
`structuredContent` so they keep the existing Claude Code response shape.

A save failure is induced using an empty directory at this private session's
screenshot destination. The response must still include the image, with
`path: null` and `write_error`. The script changes no shared quota or permissions.

Client-specific rendering is not inferred from the protocol checks. Clients that
hide images alongside structured results should use the unchanged default.

On engi, all nine capture cases passed in the fixed build (exit 0;
[output](after.log)). The unchanged baseline passed the file, default-inline,
and explicitly-disabled cases, then failed at the opted-in inline case with
`key not found: "structuredContent"` (exit 1). Both sessions stopped and their
workdirs were cleaned up.
