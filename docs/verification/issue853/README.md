# Issue 853: navigation must establish the destination

Verified on engi with KWin 6.8.0, Google Chrome 156.0.8078.47, pasta,
bubblewrap, EIS/input devices, and branded Chrome using an explicit private
profile with CDP. All desktop input went through the built production stdio
server. The test ran under the host user service manager to avoid the calling
sandbox's NoNewPrivs restriction.

## Reproduction

The baseline was built from `cc362b3`. The reported DigiKey search URL
(`https://www.digikey.com/en/products/result?keywords=TwinMax`) ended at the
coaxial-cables filter URL from the issue comment. The baseline returned
`matched:false` with `isError:false` and called it `navigated`. Native omnibox
copy confirmed the different destination. The fixed build returns
`isError:true`, `status:not_reached`, the requested and observed URLs, and
`retry_safe:false`. DigiKey to the reported AliExpress URL succeeded in the
existing tab on both settled runs; the fixed result explicitly verified its
committed URL. An earlier baseline run also returned the old DigiKey title
while native copy already showed the pending AliExpress address.

The local regression makes the stale-page outcome deterministic: request a
file download from an existing page. Chrome receives the download but keeps
the previous document and address. The baseline reported `navigated`,
`matched:false`, and `isError:false`; the fixed build reports a tool error.
Both redirect cases must also be errors, including a redirect differing only
in its query. This does not assume that every different destination is a
history completion: a site can redirect or a navigation can remain pending.

## Root causes and changes

- Ctrl+L was followed by a fixed delay and blind typing. The tool now requires
  a uniquely focused native `OmniboxViewViews` field with its complete value
  selected, retries focus with Alt+D, watches the field/window while typing,
  removes inline completion, and verifies the exact address before Return.
  The native widget class is independent of the translated accessible name;
  a focused web editor cannot qualify.
- A title change was treated as navigation completion, and CDP pages were
  associated afterward through their changing titles. The tool now binds the
  original tab before submission and polls its committed navigation history.
  It does not evaluate JavaScript in a renderer that may be navigating.
- URL comparison discarded queries/fragments and lowercased paths. These
  components now remain significant and case-sensitive.
- Only a verified destination is successful. Mismatches, unchanged pages and
  unavailable CDP verification produce tool errors. A pending navigation may
  still finish after the eight-second wait; the result explicitly requires
  inspection and does not recommend blind retries. Redirects to different
  destinations are deliberately errors for the same reason.

## Regression and evidence

```bash
cargo test --test browser_navigate_e2e -- --ignored --exact \
  navigate_opens_the_typed_address_and_reports_where_it_landed --nocapture
cargo test --bin kwin-mcp navigate_urls_are_checked_and_compared_by_destination
cargo clippy --bin kwin-mcp --test browser_navigate_e2e
```

The real-system regression independently verifies addresses with native
Ctrl+L/Ctrl+C and the session clipboard, plus host HTTP request receipts. It
covers a populated tab across origins, inline history completion, case-sensitive
queries, delayed pages, unchanged titles, a download, two redirects, invalid
input, and New Tab navigation. The fixture uses no page scripts or external
services, and cleans up its session and private HOME.

- [Baseline receipts](baseline.json): real-system failure at the assertion
  rejecting normal success for the download's unchanged page.
- [Fixed receipts](fixed.json): all cases passed, including New Tab.
- [Reported sites before](reported-sites-before.json) and
  [reported sites after](reported-sites-after.json): real-site calls and
  independent omnibox copies.

The URL-comparison test passed. Targeted Clippy passed with the existing
`PreparedPaste` large-enum warning. Repository-wide `cargo fmt --all -- --check`
reports pre-existing formatting differences in untouched code; formatting
changes are restricted to the modified sections and navigation test.
