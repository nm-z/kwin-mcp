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

## Bambu Studio

Taken by hand through the stdio server, in a 1920x1080 session whose private
HOME held a copy of Bambu Studio's settings, with no login and no network
plugin. Bambu Studio 2.9.1.70 (AppImage), Preferences dialog: 640x640 at
(640,220), modal and transient for the main window. Its Language dropdown is a
wxWidgets popup window, as the report's filament mapping is. It was opened
with a click at (543,94), and the pointer was then moved to (100,560), so that
the 97-pixel square around it covers none of the dropdown. `window_list`
listed only the main window and the dialog.

- `before-bambu-language-refused.json` (master, 3be7567): `13549 of 379967
  opaque dialog pixels differ from the independent window capture ... still
  differing after 30 captures`. `region=[400,60,640,640]` and `cursor=true`
  were refused the same way. After a click outside closed the dropdown, the
  dialog matched again: 0 of 379967.
- `after-bambu-language.png` and `.json` (this branch): the dialog with the
  dropdown in it. `transient_pixel_check` is `matches_window_capture` on the
  first capture, 0 of 287423 compared pixels differ and 92544 are left out.
  `popups` is `[{"region":[472,114,616,532],"unlisted":true}]`: like the
  fixture's, the dropdown is an override-redirect X11 window that KWin does
  not manage. `region=[472,114,616,532]` (the whole list, 144x418) and
  `cursor=true` were not refused either.
