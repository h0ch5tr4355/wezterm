---
tags:
  - prompt
---

# `PopupMenu`

{{since('nightly')}}

Shows a drop-down menu drawn by the GUI. When a clickable part of the
status bar was clicked just before (see
[window:set_right_status](../window/set_right_status.md#clickable-parts)),
the menu opens underneath that part, like a menu of the macOS menu bar.
Otherwise it opens underneath the tab bar at the right edge of the window.

The menu is used with the mouse (pointing highlights an item, a click
chooses it, a click elsewhere closes the menu, the wheel scrolls a long
menu) or with the keyboard (`UpArrow`/`DownArrow`, `Home`/`End`, `Enter`
or `Space` chooses, `Escape` closes). Clicking another clickable part of
the status bar while a menu is open switches to that part's menu.

`PopupMenu` accepts the following fields:

* `entries` - a lua table of the rows, from top to bottom. Each row is a
  table with these fields, all optional:
    * `kind` - `"Item"` (the default), `"Header"` (a dimmed section title)
      or `"Separator"` (a horizontal rule)
    * `label` - the text of the row
    * `id` - passed to the action. An `Item` without an `id` is shown
      disabled and cannot be chosen
    * `detail` - text at the right edge of the row
    * `icon` - text in a narrow column ahead of the label, for example a
      glyph from [wezterm.nerdfonts](../wezterm/nerdfonts.md)
    * `badge` - short text in a rounded box after the label, such as `"PROD"`
    * `keys` - a shortcut such as `"Ctrl+Shift+T"`, drawn as one key cap per
      key (split at `+`). The shortcuts of all rows start in the same column
    * `progress` - a number from `0.0` to `1.0`; shows a bar filled to that
      fraction. The bars of all rows line up
    * `image` - the path of a picture (PNG, JPEG, GIF, ...). The row becomes
      a card: the picture at the left, the label, each line of `detail` and
      the progress bar stacked next to it. A file that cannot be read is
      left out
    * `image_size` - height (and width) of the picture in lines, default 4
    * `color`, `detail_color`, `icon_color`, `badge_color`, `progress_color` -
      colors of these parts
* `action` - an event callback registered via `wezterm.action_callback`. It
  is called with `(window, pane, id, label)` of the chosen row, or with
  `id` and `label` set to `nil` when the menu was closed without a choice.
* `bg_color`, `fg_color` - colors of the menu; they default to
  [command_palette_bg_color](../config/command_palette_bg_color.md) and
  [command_palette_fg_color](../config/command_palette_fg_color.md)
* `dim_color` - color of headers, details and disabled items
* `highlight_color` - background of the highlighted item
* `border_color` - color of the frame and the separators
* `min_width` - the minimum width of the menu in cells

The text uses [command_palette_font](../config/command_palette_font.md) and
[command_palette_font_size](../config/command_palette_font_size.md); headers
use the font of the tab bar.

Since the menu belongs to the GUI, it works the same with every domain.
Use [wezterm.has_action](../wezterm/has_action.md) to fall back to
[InputSelector](InputSelector.md) on versions that lack `PopupMenu`.

```lua
local wezterm = require 'wezterm'
local act = wezterm.action
local config = wezterm.config_builder()

config.keys = {
  {
    key = 'm',
    mods = 'CTRL|SHIFT',
    action = act.PopupMenu {
      entries = {
        { kind = 'Header', label = 'Kubernetes' },
        { label = 'Context', detail = 'staging', id = 'copy' },
        { label = 'Quota', progress = 0.42, detail = '42%' },
        { kind = 'Separator' },
        { label = 'Show pods', icon = wezterm.nerdfonts.md_cube_outline, keys = 'Ctrl+Shift+P', id = 'pods' },
        { label = 'production', badge = 'PROD', badge_color = '#c4483c', id = 'prod' },
      },
      action = wezterm.action_callback(function(window, pane, id, label)
        if id == 'copy' then
          window:copy_to_clipboard 'staging'
        elseif id then
          wezterm.log_info('chose ' .. id)
        end
      end),
    },
  },
}

return config
```
