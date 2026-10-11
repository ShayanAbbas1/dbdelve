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

A file there is either a Zed theme file or one DBDelve theme, whose colours are
named after DBDelve's own tokens (`bg`, `panel`, `surface`, `overlay`,
`control`, `text`, `text_muted`, `accent`, `selection`, `syntax_keyword` and
the rest in [`src/theme/mod.rs`](../src/theme/mod.rs)). Any left out come from
DBDelve Dark or Light:

```json
{
  "name": "Harbor",
  "appearance": "dark",
  "glass": false,
  "colors": { "bg": "#1b2430", "text": "#d8dee9", "accent": "#88c0d0" }
}
```

A theme named like one DBDelve ships is skipped. Restart DBDelve to pick up a
file you changed by hand.
