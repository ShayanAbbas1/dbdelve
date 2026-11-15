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

DBDelve adds Tab acceptance through `views::completion_target`, attached to the
query buffer and each filter value so the action always names the editor with
focus, not the tab's runnable buffer. The binding uses `Input && mode == editor`:
`Editor > Input` depends on the query surface's wrapper and misses raw filters.
Both contexts apply in a query buffer, so shortcut conflict detection must
recognize their overlap when completion or Toggle Line Comment is rebound.
After accepting through the menu's synthetic Enter, stop propagation, because
the menu propagates and would otherwise run the original keystroke as well.
With no suggestion, propagate the original stroke to its native binding: Tab
indents, while a completion shortcut rebound to Enter still submits a filter.
The filter consumes a propagated native Enter after its submission event so
the unhandled-keystroke fallback cannot insert a hidden newline.

`ctrl-space` (`show_completions`) dispatches the library's own
`ShowCompletions`, which it handles but leaves unbound, under the same
`Input && mode == editor` context. On macOS the system takes that chord first
while more than one input source is enabled; it can be rebound.

The library binds `escape` scoped to the input; DBDelve's `show_editor` binding
is unscoped, and an unscoped binding wins at every depth (see "GPUI hazards" in `AGENTS.md`).
`show_editor` must therefore `cx.propagate()` on the paths where DBDelve has
nothing stacked to close, or the completion popup cannot be dismissed, and
first of all while a `PopupMenu` holds the keyboard, or `escape` in a
dropdown takes down the dialog or form under it. Its two
last stops are focus moves, not dismissals: from the sidebar's tree it hands
the keyboard to the surface in front, and from a query tab's grid (the table
itself, never a cell's open input, which is its descendant) to the buffer
above it, keeping the selection. Both run before the propagate, so on a query
tab the table's own `escape`, which clears its selection, never sees it; on an
object tab's grid, with no buffer to go to, it still does.

It does **not** provide a fuzzy matcher, and its `command` palette filters by
substring in a fixed layout. Both are ours: `src/fuzzy.rs` is the one matcher
(`nucleo-matcher`) every list that narrows as it is typed into goes through --
the palette and its pickers (the connection form's engine among them), completions, the explorer's filter, the connections filter over the complex sidebar (`explorer::Builder`, over every loaded subtree and the connection, database and project names; it never connects or loads anything, and a connection that has not loaded is kept only by its own name) and the settings search -- so a query that finds something in one finds it in the others, and
each draws the matched characters with `ui::highlighted`. The connections tree's `Loading…` row is static text, not a spinner: nothing new moves. A palette row carries a `Command`; `Workspace::run_command` routes every one into
the method its button or keystroke already calls, so the palette is never a
second implementation of anything. What a sidebar node offers is one list, `sidebar::node_actions` (New Query Here, Copy Name, Show DDL, Refresh, as its kind and target allow): both trees' menus and the Commands scope's rows for the selected node read it, as `explorer::relation_copies` is the one list of a relation's copies. A node row carries the node it was built for, and `run_node_action` brings that connection forward before it acts. Show DDL is a read-only dialog, never a buffer, and no node action sends a statement dbdelve writes. The connections tree (complex mode only; simple mode keeps one selected row) also keeps a selection of several rows: a Cmd-click (Ctrl elsewhere) adds or removes a row and a Shift-click selects every showing row from the last one clicked, while a plain click is the one row again. The set is DBDelve's (`ConnectionsSidebar.selection`, by row id, kept across rebuilds and dropping rows that no longer exist): `TreeState` selects one row, which is the anchor. With several selected, the menu and the Commands scope offer what `sidebar::selection_actions` says applies to every row (Open All, Copy Names, Copy DDL, Show DDL, Pin, Refresh), under `N tables selected`; Show DDL lists each definition in the one dialog and no selection action sends a statement dbdelve writes. `cmd+p` (`FuzzyOpen`, "Go to Object")
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


A connection row in the complex sidebar reads, left to right, logo (in the
connection's colour; an uncoloured connection's logo wears its engine's
brand), name, host (`ConnectionConfig::endpoint()`, which gives way first: it
takes only the width the name leaves and shortens with an ellipsis down to
nothing before the name shortens), count and status dot, with a coloured
connection's whole row filled with `ConnectionColor::fill()` under the
selection and hover `ListItem` draws over it. The dot is the connection's own
state (`ProfileState`). `a_tinted_sidebar_row_reads_in_every_theme` holds the
fill to the floors the other tints keep; `Theme::text_faint` is under any floor
on some surfaces untinted, so the host and the count are held to what a tint
takes from them instead. While the filter has text, a line under the tree
counts the listed connections that have not loaded, which it could not search.

Settings > General > Sidebar carries three display settings, stored in
`[settings]` (`sidebar_density`, `sidebar_show_host`, `sidebar_show_sizes`, each
absent when never set). Density (Compact, Standard, Comfortable: row height and
indent step 22/12, 26/14 and 30/16 px) is the connections tree's alone: the
simple explorer keeps its own rows whatever it is, since no density reproduces
its row exactly (its indent step is 12 px, Compact's, but not its height). Show host drops the host from connection rows. Show sizes off
draws no size in either tree and means no read: `load_sizes` and the database
sizes step return before sending `sizes()`, and turning it on again reads every
connected profile and loaded database (`sidebar::shown_size` decides the slot;
the agent's own catalog read is not part of it).

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

## Painting by hand: the ER diagram

A thousand cards as divs would be a thousand layouts a frame, so the diagram
(`diagram_view.rs`) is the one surface that paints itself, and the pattern is
the model for any other that must.

- **One `canvas()`**, whose paint closure draws everything in the pane: dots,
  edges, cards, rows, text. Cards and rows are quads, edges are tessellated
  meshes cached per edge and look, and zoom is our own transform over world
  coordinates, never a scaled element.
- **Few paths, few draw orders.** gpui batches primitives of one kind only
  while their draw orders run unbroken, and every `Path` carries its own
  vertex buffer. The edge meshes are merged into a handful of `Path`s a
  frame, one per look and part (ink, hole, ring) in chunks of
  `PATH_VERTICES`, and the dot grid and each card paint inside
  `window.paint_layer`, so a full diagram is a few batches rather than
  thousands. A crowded diagram (`CROWDED` edges) also flattens its
  faded edges coarser, keeping a 5K frame under gpui's instance-buffer cap.
- **Cull through the scene.** `diagram::index::Scene` files every card and
  edge segment in a uniform grid in world space; a frame asks it for
  `visible(world, zoom)` and paints only that, and a mouse move looks in one or
  two cells rather than scanning. Dragging or resizing refiles the one card.
- **Shape text once.** `ShapedLine`s are cached by text, size step and ink
  role, and the whole cache is dropped when the theme, opacity or font
  changes (`Stamp`), since each line holds its colour and family. Sizes are
  quantized to a few steps because gpui's glyph atlas never evicts a size.
  Labels never go under 11 px.
- **A diagram behind another tab holds nothing only drawing needs.** When
  another tab or profile comes to the front (`Workspace::track_shown_diagram`,
  checked each render), `DiagramView::hide` drops the shaped lines, the
  tessellated edges and both scenes' grids (`Scene::release`), keeping the
  graph, the layouts and the cards' places; the next paint files the grid
  again (`Scene::refile`).
- **Nothing animates.** No timer, no repeating animation (`AGENTS.md`, GPUI
  hazards): a frame is drawn when a gesture or a load notifies the view, and the
  view is mounted cached so a redraw elsewhere reuses its last frame. Idle is
  0% CPU.
- **Chrome sits on top as ordinary elements**: the toolbar, find field,
  minimap and focus bar are divs over the canvas, each through
  `ui::frosted_card`, which `occlude`s so a press on one never reaches the
  canvas. Colours come from `theme(cx)` and `popup_theme(cx)` only; the
  diagram's pairings are held to contrast floors in
  `the_diagrams_inks_hold_on_its_cards_and_canvas_in_every_theme`.
- **What lives where.** `diagram/` (`layout`, `geometry`, `index`) is pure:
  world units, no gpui, run on a background thread and tested without a window.
  `diagram_view.rs` owns everything gpui: the transform, the pointer, the
  caches, the paint. `workspace/diagrams.rs` reads the schema graph and
  persists the arrangement. Put new geometry in `diagram/` with a test, and keep
  the view to drawing it.

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
  surface comes on screen, never on a re-render, so it plays again, without
  the greeting, when Disconnect brings the surface back; `reduce_motion`
  skips it.
  The URL field's engine logo fades in once over 160ms, keyed by the engine.
  Neither repeats, so neither is the redraw hazard in `AGENTS.md`.
- **The connection form's card** (`workspace/forms.rs`) rises in on the same
  clock and curve (`Part::Form`, read off `ConnectionForm::opened`), over the
  same contours, which fade in only when the welcome surface was not already
  showing them (`ConnectionForm::backdrop_shown`). It stops asking for frames
  at `form_entrance_ms`, and `reduce_motion` skips it. Its SSH and AWS SSM rows settle in
  once over 220ms when the tunnel is switched on (`revealed`, keyed by the
  transport and row, so switching transports replays the entrance). Neither repeats.

- **The approval surface moves nothing.** An agent's approval dialog has no
  animation, no smooth scroll and no countdown: the statement is read where it
  stands, and a timer that counted down on screen would be a clock to beat.
  The statement block's scrollbar is always drawn, never faded in, and on a
  window too short for the whole card the block gives up its height first so
  Deny and Approve stay on screen.
  Approve's arm is a one-shot timer (`ARM_DELAY`), not an animation, and ends
  in one redraw, which takes Approve's label from the faint ink to the quiet
  one: the library's disabled style dims only a fill, and a ghost has none. The dialog asks the OS for attention once per call, when it is
  first drawn (`Window::request_attention`): a single dock bounce or taskbar
  flash that never takes focus from another application and never raises the
  window, and nothing at all on Wayland, where the mark's waiting count is the
  signal.

Anything else that moves needs raising first, and a second repeating animation
means building the shared throttled clock and moving both onto it.

