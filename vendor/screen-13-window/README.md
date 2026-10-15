# Screen 13 Window Helper

Vendored from `screen-13` commit `d9f961c` for kwin-mcp's host viewer.

The local patch waits for the first nonzero `WindowEvent::Resized` before
requesting or drawing a frame. On Wayland, that event follows the initial xdg
surface configure; presenting before it can leave Winit waiting on a frame
callback that the compositor never sends.
