# UI building blocks

## What gpui-component provides

Use these rather than hand-rolling: `EditorState` (the SQL buffer: rope-backed,
IME, line numbers, tree-sitter highlighting), `DataTable` over a `TableDelegate`
(the grid, virtualized on both axes), `v_virtual_list` (the palette's rows,
virtualized at their two heights, header and row), `PopupMenu` and the `DropdownMenu`
trait (an anchored menu whose open state the library owns, which is the answer
to the "GPUI drops view state" hazard under "Animation hazards" below rather than a dropdown of ours),
`Root` dialog layers, resizable panels, `Spinner`, and the completion popup.

Popup menus use the component theme's chrome font, even when opened from an
editor or grid. The vendored `PopupMenu` resets the inherited family at its root
so labels and submenus do not pick up the triggering surface's monospace font.

**Completion is the library's.** `CompletionProvider` is the trait and the
editor's `lsp.completion_provider` is a public field. No language server is
involved: `lsp_types` is the vocabulary and nothing starts a process. The editor
draws the menu itself, so a provider is the whole integration, and `up`, `down`,
`enter` and `escape` are already routed to the menu when it is open. Do not
hand-roll a popup beside it.

The library binds `escape` scoped to the input; DBDelve's `show_editor` binding
is unscoped, and an unscoped binding wins at every depth (see "GPUI hazards" in `AGENTS.md`).
`show_editor` must therefore `cx.propagate()` on the paths where DBDelve has
nothing stacked to close, or the completion popup cannot be dismissed.

It does **not** provide a fuzzy matcher, and its `command` palette filters by
substring in a fixed layout. Both are ours: `src/fuzzy.rs` is the one matcher
(`nucleo-matcher`) every list that narrows as it is typed into goes through --
the palette and its pickers (the connection form's engine among them), completions, the explorer's filter and the settings search -- so a query that finds something in one finds it in the others, and
each draws the matched characters with `ui::highlighted`. A palette row carries a `Command`; `Workspace::run_command` routes every one into
the method its button or keystroke already calls, so the palette is never a
second implementation of anything. `cmd+p` (`FuzzyOpen`, "Go to Object")
opens it on Objects, `cmd+shift+p` on Commands, `SearchEverything` (no default chord) on All and the titlebar's project or
connection on Connections; the stroke that opened it, or the one for the scope it
shows, closes it, and another scope's stroke moves it there (`palette::stroke`).
`tab` and `shift-tab` step through the scopes, and so do `right` and `left`
once the caret sits bare at the end or the start of the field, an empty one
included (`palette::arrow`). Anywhere else, and in every picker, the arrows
are the field's own: the palette's handler propagates, and GPUI hands the
keystroke to the next binding in line, the input's caret movement.
A palette open while the catalog loads says so on Objects, and takes the
objects in when they land (`Workspace::catalog_landed`). One opened from the
settings panel, or put up by a row confirmed there, hands the keyboard back to
the panel's search when it is dismissed or a theme or font is picked from it;
any other row it confirms leaves focus where its command put it.

It also does **not** ship the icons its `IconName` names: those are Lucide file
paths with no files behind them. `src/icons.rs` is DBDelve's `AssetSource`: it
serves the same paths from `icondata_lu` in memory, so nothing is vendored and
the library's own widgets get their icons from it too. Add a row to `ICONS`
when something needs one; an unlisted path draws nothing.

Its `Button` is worth using for the mechanism (tooltip, focus ring, disabled
gate) and nothing else. **Never construct one directly**: go through
`ui::button`, `ui::icon_button` and `ui::button_label`, which measure the box
off the layout scale (`CONTROL_HEIGHT`) and put the label and icon in as
children carrying their own colour, so the library's hover tint does not reach
them. GPUI's `.hover()` panics if called twice, so there is no fixing it from
outside.

The library owns scroll math, text shaping and virtualization. **Every visible
pixel is still ours**: `TableDelegate::render_td` is pull-based, and the
highlighter emits neutral token kinds that we map to our own palette. Never
accept a library default appearance; map it to our theme tokens.

---


## Titlebar and appearance

- **Titlebar**: macOS and Windows get DBDelve's own strip over a transparent
  system titlebar (`ui::CLIENT_TITLEBAR`); Windows draws no buttons into it, so
  `ui::titlebar` draws them. Linux keeps the system bar.
- **Appearance**: with Match system appearance on, the window wears
  `Settings::light_theme` or `dark_theme` by the system's side, switched by
  `cx.observe_window_appearance` (`follow_system_appearance`). All three
  platforms report the change; Linux reads it from the XDG portal's
  `color-scheme`, which can answer after the first frame, and the observer
  catches that too. The theme global is still the one source for what is on
  screen; the two names say what to put on next (`Settings::system_theme`).
  Select Theme opens the list for the side the system is on now, so a side is
  only offered themes of its own appearance and the theme picked is the theme
  shown. A change that lands while a theme list is previewing goes into
  `theme_before_preview` and the palette's fallback, never over the preview.

## Animation hazards

The redraw hazard, the one that costs CPU, is in `AGENTS.md`, "GPUI hazards". The rest:

- **`with_animation` replays from zero on remount.** Anything that must survive
  being unmounted mid-animation needs a wall-clock-driven tween evaluated fresh
  each render, not an element-id-keyed animation.
- **GPUI drops view state the same frame the view unmounts.** Exit animations
  need an explicit open → closing → closed lifecycle plus a reaping timer.
  Every dropdown, toast and modal hits this.
- **No scale transform on `div`.** SVG only at this revision. Approximate with
  fade plus translate.
- **`translateY` is a relative-position inset** applied after layout, so siblings
  do not shift.
- **`.hover()` snaps with no transition**, so hover states are instant
  everywhere. A colour fade would have to be hand-driven from a wall clock.

## Animation

There is no motion system of DBDelve's own: no transitions, no shared easing
curves, and hover states are instant. These things move:

- **The query-in-flight `Spinner`** (gpui-component's, built in `views.rs`),
  shown while a query runs beside a live **Cancel** button. It is
  `Animation::new(speed).repeat()` internally, which makes it exactly the first
  hazard under "GPUI hazards" in `AGENTS.md`, and **the throttled-clock remedy is not in place**: one spinner
  per running tab, mounted while `QueryState::Running` and dropped when the
  result lands. Tolerable because it is transient and few tabs run at once, but
  an unmeasured ceiling, not a solved problem. One case is not transient: a
  preview tab in `QueryState::Idle` shows the same spinner, so a preview that
  never runs spins forever.
- **The row panel's "copied" check** fades in once over 150ms. It does not
  repeat, so it is not the redraw hazard in `AGENTS.md`.
- **The welcome surface's entrance** (`workspace/welcome.rs`), approved for
  that screen alone. The mark, the title, the card and the engine tiles fade
  and rise a few pixels into place (opacity and a relative `top`, never
  scale), the contours behind fade in, and on the very first run a greeting
  holds the title's place for a moment first (`Settings::welcomed`, persisted).
  One wall clock (`Entrance`) drives every piece rather than an element-keyed
  animation each: the surface asks for the next frame only until
  `entrance_ms` has passed and then asks for nothing. It starts when the
  surface comes on screen, never on a re-render, and `reduce_motion` skips it.
  The URL field's engine logo fades in once over 160ms, keyed by the engine.
  Neither repeats, so neither is the redraw hazard in `AGENTS.md`.
- **The connection form's card** (`workspace/forms.rs`) rises in on the same
  clock and curve (`Part::Form`, read off `ConnectionForm::opened`), over the
  same contours, which fade in only when the welcome surface was not already
  showing them (`ConnectionForm::backdrop_shown`). It stops asking for frames
  at `form_entrance_ms`, and `reduce_motion` skips it. Its SSH rows settle in
  once over 220ms when the tunnel is switched on (`revealed`, keyed by the
  row, so they replay only on remounting). Neither repeats.

Anything else that moves needs raising first, and a second repeating animation
means building the shared throttled clock and moving both onto it.

