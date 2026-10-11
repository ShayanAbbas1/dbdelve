# Themes

DBDelve can import themes from Zed and VS Code, and reads theme files you write
yourself.

## Importing

Open the theme picker and choose **Import from Zed…** or **Import from VS
Code…** at the end of the list.

- Zed: its own defaults, the ones in Zed's themes folder
  (`~/.config/zed/themes`, under `$XDG_CONFIG_HOME` on Linux if set, or
  `%APPDATA%\Zed\themes` on Windows) and those of every installed Zed
  extension. Themes with a blurred or transparent background come out as glass.
- VS Code: the themes VS Code ships with, read from its install, and those of
  every extension in `~/.vscode/extensions`.

Each is previewed as you move through the list, and Enter imports it: DBDelve
converts it into a theme file of its own and switches to it. The imported
theme's name is prefixed with the app it came from, as in "Zed: One Dark" or
"VS Code: Monokai".

A theme already imported is marked so. Importing it again overwrites the file
that holds it. An import never overwrites a file holding a different theme; it
picks a free filename instead.

## Writing your own

Imported themes, and any you write yourself, live in a `themes` folder next
to DBDelve's other data:

- macOS: `~/Library/Application Support/dbdelve/themes`
- Linux: `$XDG_DATA_HOME/dbdelve/themes`, or `~/.local/share/dbdelve/themes`
- Windows: `%APPDATA%\dbdelve\themes`

A file there is either a Zed theme file or one DBDelve theme, a `.json` file
like this one. It is DBDelve Dark, so you can save it as `harbor.json` and
change the colours from there:

```json
{
  "name": "Harbor",
  "appearance": "dark",
  "glass": false,
  "colors": {
    "surface": "#191b1c",
    "bg": "#2d2e30",
    "panel": "#232426",
    "overlay": "#393b3d",
    "control": "#414244",
    "text": "#e6e8ea",
    "text_muted": "#afb1b4",
    "text_faint": "#858689",
    "accent": "#4ba3f7",
    "on_accent": "#08090b",
    "cursor": "#59aaf8",
    "danger": "#ff645f",
    "success": "#53be70",
    "syntax_comment": "#97989b",
    "syntax_keyword": "#c4a4fe",
    "syntax_string": "#76cf8a",
    "syntax_number": "#f2b966",
    "syntax_function": "#7cbdff",
    "syntax_type": "#65d0dc",
    "syntax_variable": "#dcdee1",
    "syntax_operator": "#a9abad",
    "element_hover": "#ffffff0d",
    "element_active": "#ffffff17",
    "border": "#ffffff17",
    "border_strong": "#ffffff29",
    "selection": "#449df047",
    "edited": "#ac730052"
  }
}
```

| Field | |
| --- | --- |
| `name` | Required. Shown in the theme picker. A theme named like one DBDelve ships is skipped. |
| `appearance` | `"dark"` or `"light"`. Picks the theme any missing colour comes from: DBDelve Dark or DBDelve Light. Defaults to dark. |
| `glass` | `true` paints the window over the blurred desktop, the way the Glass themes do. Defaults to `false`. |
| `colors` | Every key is optional. Any you leave out comes from DBDelve Dark or Light. |

Colours are written `#RGB`, `#RGBA`, `#RRGGBB` or `#RRGGBBAA`. A value in any
other form is ignored, and that colour falls back to DBDelve's.

| Colour | Where it shows |
| --- | --- |
| `bg` | The brightest plane: the results grid. |
| `panel` | One step behind `bg`: the editor and the active tab. |
| `surface` | Chrome, furthest back: sidebar, titlebar, status bar, table headers. |
| `overlay` | Floating panels such as the palette and the connection switcher. |
| `control` | Buttons at rest. |
| `text`, `text_muted`, `text_faint` | Body text, secondary text, and the faintest text, such as hints. |
| `accent` | The highlight colour: focus, links and active items. |
| `on_accent` | Text drawn on the accent. |
| `cursor` | The text cursor. |
| `danger`, `success` | Errors and destructive actions, and success messages. |
| `syntax_*` | SQL highlighting: `comment`, `keyword`, `string`, `number`, `function`, `type`, `variable` and `operator`. |
| `element_hover`, `element_active` | Washes under whatever the pointer is over, and under the active tab or chip. |
| `border`, `border_strong` | Dividers, and outlines that must stand out. |
| `selection` | Selected text and selected grid cells. |
| `edited` | Grid cells changed but not yet applied. |

The colours from `bg` to `syntax_operator` are solid. If you give one an alpha,
it is flattened over `surface`. The last six are washes painted over whatever
sits under them, so give them a low alpha, as above.

Restart DBDelve to pick up a file you changed by hand.
