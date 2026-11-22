# `window:get_viewport_top_for_pane(pane)`

{{since('nightly')}}

Returns the [StableRowIndex](../pane/get_dimensions.md) of the first row
that the window shows of the specified pane while it is scrolled back, or
`nil` when it shows the bottom of the scrollback.

Like the selection, the scroll position belongs to the window, which is why
this is not a method of the `pane` object.

This example copies what is visible in the pane to the clipboard, also when
it is scrolled back:

```lua
local wezterm = require 'wezterm'

wezterm.on('copy-visible', function(window, pane)
  local dims = pane:get_dimensions()
  local top = window:get_viewport_top_for_pane(pane) or dims.physical_top
  local text = pane:get_text_from_region(
    0,
    top,
    dims.cols,
    top + dims.viewport_rows - 1
  )
  window:copy_to_clipboard(text, 'Clipboard')
end)
```
