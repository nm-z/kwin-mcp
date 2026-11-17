# #983: a dialog's own dropdown in its screenshot

Kept by `tests/dialog_popup_capture_e2e.rs` with `KWIN_MCP_E2E_EVIDENCE`, in
1280x800 sessions with `window_mode=native`. The fixture
(`tests/fixtures/dialog_dropdown.c`) shows a blue main window and a green
520x420 dialog transient for it at (380,190); its dropdowns are magenta.

## Before (master, 3be7567)

- `before-x11-dropdown-refused.json`: under X11 (`GDK_BACKEND=x11`, as Bambu
  Studio runs) the dialog's button opens an override-redirect dropdown made
  like wxWidgets' `wxPopupTransientWindow`. Every screenshot is refused:
  `36863 of 194207 opaque dialog pixels differ from the independent window
  capture ... still differing after 15 captures`, the report's error.
- `before-wayland-menu-refused.json`: the same refusal for a GTK combo box's
  menu, an xdg_popup, under Wayland (41763 pixels: the menu and its shadow).

## After

- `after-x11-dropdown.png` and `.json`: the default screenshot is the dialog
  itself, 520x420 and window-relative, with the dropdown in it.
  `transient_pixel_check` is `matches_window_capture` on the first capture,
  with 0 of 140864 compared pixels differing and 53343 pixels of the dropdown
  and its margin left out. `popups` is `[{"region":[140,258,380,478],
  "unlisted":true}]`: `window_list` lists only the main window and the dialog.
  The text says the dropdown reaches past the image.
- `after-x11-dropdown-region.png` and `.json`: `region=[140,258,380,478]`
  shows the whole 240x220 dropdown, including the 58 rows below the dialog.
- `after-wayland-menu.png` and `.json`: the combo box's menu, reported with its
  `window_list` ID and region `[140,193,380,387]`, 0 differing pixels.
- `after-other-program-refused.json`: a second process's override-redirect
  window over the dialog is not the dialog's own, and the screenshot is still
  refused with `transient_capture_unverified`, as before the fix.
