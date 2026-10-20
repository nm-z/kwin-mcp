# Chrome Load unpacked visual proof

The documented directory chooser workflow was exercised through the built production
stdio server on engi, using a fresh Chrome profile in an isolated KWin desktop.

1. A generated Manifest V3 extension named "Wingnut folder probe 375", version 1.0,
   was created inside the session.
2. Chrome opened chrome://extensions; Developer mode was enabled.
3. Load unpacked opened the native GTK directory chooser.
4. Ctrl+L and keyboard_type entered the absolute session-visible directory path.
5. A screenshot confirmed the path; the native pointer clicked the top-right Open.
6. A later screenshot confirmed the named card with its enabled toggle.

The raw images are folder-chooser.png and enabled-extension.png. A closed chooser
and an immediately returned browser frame were insufficient evidence; verification
continued until the card was visibly present. The Chrome internal page did not
provide find_ui_elements matches, so visual inspection established the result.

The GUI binary was built from a54f721, with the same launch, input and screenshot
implementation as the current documentation branch f7b6ac4. The latter preserves
the bounded server initialization text. This proof does not add an input-delivery
implementation change.

Only a generated extension and private profile were used. The extension has no
background service or relay; no claim is made about extension-specific network
behavior. Application sign-in was not involved.
