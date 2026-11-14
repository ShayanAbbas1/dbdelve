//! The diagram as an SVG file: what the canvas draws at 100%, in the theme's
//! opaque tones, wherever the user left each card and whatever size they gave
//! it. Text is `<text>` in the app's families, so the file stays editable and
//! searchable; the PNG export sets it from the same document.

use std::borrow::Cow;
use std::fmt::Write;

use crate::db::{GraphColumn, KeyTarget, SchemaGraph};
use crate::theme::color::Srgb;

use super::geometry::{Detail, HEADER_HEIGHT, ROW_HEIGHT, Rect, Segment, Stroke, type_room};
use super::layout::{Layout, Subject};

/// Room around the drawn diagram, in pixels at 100%.
pub const MARGIN: f32 = 32.0;
const CARD_PAD: f32 = 10.0;
const ICON_GAP: f32 = 6.0;
const EDGE_WIDTH: f32 = 1.25;
const ELLIPSIS: char = '…';

/// The tones a file is drawn in, every one opaque: glass has no backdrop in
/// a file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub canvas: Srgb,
    pub card: Srgb,
    pub header: Srgb,
    pub border: Srgb,
    pub text: Srgb,
    pub muted: Srgb,
    pub edge: Srgb,
    pub primary: Srgb,
    pub foreign: Srgb,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub palette: Palette,
    /// The chrome family, which names are set in.
    pub chrome: String,
    /// The grid family, which types are set in.
    pub grid: String,
    pub name_size: f32,
    pub type_size: f32,
}

/// What a level of detail at 100% draws: every column, or with Keys only on,
/// the key columns.
pub fn detail(layout: &Layout) -> Detail {
    layout.detail(1.0)
}

/// What the file covers, in world units: the drawing and its margin.
pub fn frame(layout: &Layout) -> Rect {
    layout.bounds().inflate(MARGIN)
}

/// The name a card shows: its table's, or a stub's schema-qualified one.
pub fn card_name(graph: &SchemaGraph, subject: Subject) -> Cow<'_, str> {
    let table = |table: u32| Cow::Borrowed(&*graph.tables[table as usize].name);
    match subject {
        Subject::Table(index) => table(index),
        Subject::Stub(key) => match &graph.keys[key as usize].referenced {
            KeyTarget::Elsewhere { schema, table } => Cow::Owned(format!("{schema}.{table}")),
            KeyTarget::Table { table: index, .. } => table(*index),
        },
    }
}

/// The SVG document for `layout`, a layout of `graph`.
pub fn svg(graph: &SchemaGraph, layout: &Layout, style: &Style) -> String {
    let detail = detail(layout);
    let frame = frame(layout);
    let p = &style.palette;
    let mut out = String::with_capacity(512 + layout.cards.len() * 1024);
    let (chrome, grid) = (
        family(
            &style.chrome,
            "system-ui, -apple-system, 'Segoe UI', sans-serif",
        ),
        family(&style.grid, "ui-monospace, Menlo, Consolas, monospace"),
    );
    let icon_size = icon_size(style);
    _ = write!(
        out,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" xml:space="preserve" width="{w}" height="{h}" viewBox="{x} {y} {w} {h}">
<title>{title}</title>
<style>
.card{{fill:{card}}}
.head{{fill:{header}}}
.edge{{fill:{edge_fill}}}
.outline{{fill:none;stroke:{border};stroke-width:1}}
.edges{{fill:none;stroke:{edge};stroke-width:{edge_width};stroke-linecap:round;stroke-linejoin:round}}
.name{{font-family:{chrome};font-size:{name_size}px;fill:{text}}}
.title{{font-weight:600}}
.type{{font-family:{grid};font-size:{type_size}px;fill:{muted};text-anchor:end}}
.more{{font-family:{chrome};font-size:{name_size}px;fill:{muted}}}
</style>
<defs>
<symbol id="pk" viewBox="0 0 24 24" fill="none" stroke="{primary}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{pk}</symbol>
<symbol id="fk" viewBox="0 0 24 24" fill="none" stroke="{foreign}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{fk}</symbol>
</defs>
<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{canvas}"/>
"#,
        x = num(frame.x),
        y = num(frame.y),
        w = num(frame.width),
        h = num(frame.height),
        title = escape(&graph.schema),
        card = hex(p.card),
        header = hex(p.header),
        edge_fill = hex(p.canvas),
        border = hex(p.border),
        edge = hex(p.edge),
        edge_width = num(EDGE_WIDTH),
        name_size = num(style.name_size),
        type_size = num(style.type_size),
        text = hex(p.text),
        muted = hex(p.muted),
        primary = hex(p.primary),
        foreign = hex(p.foreign),
        canvas = hex(p.canvas),
        pk = icondata_lu::LuKey.data,
        fk = icondata_lu::LuLink.data,
    );

    out.push_str("<g class=\"edges\">\n");
    for edge in 0..layout.edges.len() as u32 {
        let path = layout.edge_path(edge, detail, 1.0);
        out.push_str("<path class=\"key\" d=\"");
        for segment in &path.segments {
            match *segment {
                Segment::Move(to) => _ = write!(out, "M{} {}", num(to.x), num(to.y)),
                Segment::Line(to) => _ = write!(out, "L{} {}", num(to.x), num(to.y)),
                Segment::Cubic {
                    control_a: a,
                    control_b: b,
                    to,
                } => {
                    _ = write!(
                        out,
                        "C{} {} {} {} {} {}",
                        num(a.x),
                        num(a.y),
                        num(b.x),
                        num(b.y),
                        num(to.x),
                        num(to.y)
                    )
                }
            }
        }
        for mark in &path.marks {
            if let Stroke::Line(a, b) = *mark {
                _ = write!(out, "M{} {}L{} {}", num(a.x), num(a.y), num(b.x), num(b.y));
            }
        }
        out.push_str("\"/>\n");
        for mark in &path.marks {
            if let Stroke::Circle { centre, radius } = *mark {
                _ = writeln!(
                    out,
                    r#"<circle class="edge" cx="{}" cy="{}" r="{}"/>"#,
                    num(centre.x),
                    num(centre.y),
                    num(radius)
                );
            }
        }
    }
    out.push_str("</g>\n");

    for card in 0..layout.cards.len() as u32 {
        let rect = layout.card_rect(card, detail, 1.0);
        let shown = &layout.cards[card as usize];
        let radius = match detail {
            Detail::Keys => 8.0,
            _ => 10.0,
        };
        let (x, y, w, h) = (rect.x, rect.y, rect.width, rect.height);
        _ = writeln!(
            out,
            r#"<g><rect class="card" x="{}" y="{}" width="{}" height="{}" rx="{r}"/>"#,
            num(x),
            num(y),
            num(w),
            num(h),
            r = num(radius),
        );
        let head = HEADER_HEIGHT.min(h);
        let bottom_radius = match head >= h {
            true => radius,
            false => 0.0,
        };
        _ = writeln!(
            out,
            r#"<path class="head" d="{}"/>"#,
            header_path(rect, head, radius, bottom_radius)
        );
        _ = writeln!(
            out,
            r#"<rect class="outline" x="{}" y="{}" width="{}" height="{}" rx="{}"/>"#,
            num(x + 0.5),
            num(y + 0.5),
            num(w - 1.0),
            num(h - 1.0),
            num(radius - 0.5),
        );
        let name = card_name(graph, shown.subject);
        let room = w - 2.0 * CARD_PAD;
        if let Some(title) = fitted(&name, room, style.name_size, Face::Title) {
            _ = writeln!(
                out,
                r#"<text class="name title" x="{}" y="{}">{}</text>"#,
                num(x + CARD_PAD),
                num(baseline(y, head, style.name_size)),
                escape(&title),
            );
        }

        if let Subject::Table(table) = shown.subject {
            let columns = &graph.tables[table as usize].columns;
            for row in layout.rows(card, detail) {
                let column = &columns[row.column as usize];
                write_row(&mut out, graph, column, row.rect, detail, style, icon_size);
            }
            if let Some(more) = layout.more(card, detail) {
                let text_x = more.rect.x + CARD_PAD + icon_size + ICON_GAP;
                let room = more.rect.right() - CARD_PAD - text_x;
                let label = format!("+{} more", more.hidden);
                if let Some(label) = fitted(&label, room, style.name_size, Face::Chrome) {
                    _ = writeln!(
                        out,
                        r#"<text class="more" x="{}" y="{}">{}</text>"#,
                        num(text_x),
                        num(baseline(more.rect.y, more.rect.height, style.name_size)),
                        escape(&label),
                    );
                }
            }
        }
        out.push_str("</g>\n");
    }
    out.push_str("</svg>\n");
    out
}

fn write_row(
    out: &mut String,
    graph: &SchemaGraph,
    column: &GraphColumn,
    rect: Rect,
    detail: Detail,
    style: &Style,
    icon_size: f32,
) {
    let icon_x = rect.x + CARD_PAD;
    let marker = match (column.primary, column.foreign) {
        (true, _) => Some("pk"),
        (false, true) => Some("fk"),
        _ => None,
    };
    if let Some(marker) = marker {
        _ = writeln!(
            out,
            r##"<use xlink:href="#{marker}" x="{}" y="{}" width="{s}" height="{s}"/>"##,
            num(icon_x),
            num(rect.y + (rect.height - icon_size) / 2.0),
            s = num(icon_size),
        );
    }
    let name_x = icon_x + icon_size + ICON_GAP;
    let right = rect.right() - CARD_PAD;
    let mut room = right - name_x;
    if detail.types() {
        let label = graph.types[column.type_label as usize].to_string();
        let label = match column.nullable {
            true => label + "?",
            false => label,
        };
        let label_room = type_room(
            room,
            estimate(&column.name, style.name_size, Face::Chrome),
            estimate(&label, style.type_size, Face::Mono),
            ICON_GAP,
        );
        if let Some(label) = fitted(&label, label_room, style.type_size, Face::Mono) {
            _ = writeln!(
                out,
                r#"<text class="type" x="{}" y="{}">{}</text>"#,
                num(right),
                num(baseline(rect.y, rect.height, style.type_size)),
                escape(&label),
            );
            room -= estimate(&label, style.type_size, Face::Mono) + ICON_GAP;
        }
    }
    if let Some(name) = fitted(&column.name, room, style.name_size, Face::Chrome) {
        _ = writeln!(
            out,
            r#"<text class="name" x="{}" y="{}">{}</text>"#,
            num(name_x),
            num(baseline(rect.y, rect.height, style.name_size)),
            escape(&name),
        );
    }
}

/// A key or link icon's side, as the canvas sizes it at 100%.
fn icon_size(style: &Style) -> f32 {
    (style.name_size * 0.9).min(ROW_HEIGHT * 0.7)
}

/// The header's shape: rounded at the top, and at the bottom too when it is
/// the whole card (a stub, or a card cut to its name).
fn header_path(rect: Rect, height: f32, top: f32, bottom: f32) -> String {
    let (x, y, w) = (rect.x, rect.y, rect.width);
    let b = y + height;
    let mut d = format!(
        "M{} {}A{t} {t} 0 0 1 {} {}H{}A{t} {t} 0 0 1 {} {}",
        num(x),
        num(y + top),
        num(x + top),
        num(y),
        num(x + w - top),
        num(x + w),
        num(y + top),
        t = num(top),
    );
    match bottom > 0.0 {
        true => {
            _ = write!(
                d,
                "V{}A{r} {r} 0 0 1 {} {}H{}A{r} {r} 0 0 1 {} {}Z",
                num(b - bottom),
                num(x + w - bottom),
                num(b),
                num(x + bottom),
                num(x),
                num(b - bottom),
                r = num(bottom),
            )
        }
        false => _ = write!(d, "V{}H{}Z", num(b), num(x)),
    }
    d
}

/// Where text `size` high sits so it reads as centred in a band from `top`,
/// `height` tall: a baseline rather than `dominant-baseline`, which viewers
/// disagree on.
fn baseline(top: f32, height: f32, size: f32) -> f32 {
    top + height / 2.0 + size * 0.35
}

/// The faces text is measured in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Face {
    Chrome,
    Title,
    Mono,
}

/// About how wide `text` sets at `size`. A file carries no shaper, so this
/// stands in for one: generous per character class, so a label is cut a
/// little early rather than run past its card.
fn estimate(text: &str, size: f32, face: Face) -> f32 {
    let ems: f32 = text
        .chars()
        .map(|c| {
            let wide = c > '\u{2E7F}';
            match face {
                Face::Mono => match wide {
                    true => 1.2,
                    false => 0.6,
                },
                Face::Chrome | Face::Title => match c {
                    _ if wide => 1.05,
                    ' ' => 0.28,
                    'i' | 'j' | 'l' | 'I' | '.' | ',' | ':' | ';' | '\'' | '|' | '!' => 0.3,
                    'f' | 't' | 'r' | '(' | ')' | '[' | ']' => 0.38,
                    'm' | 'w' | 'M' | 'W' | '@' => 0.86,
                    'A'..='Z' => 0.68,
                    '0'..='9' => 0.58,
                    _ => 0.56,
                },
            }
        })
        .sum();
    let weight = match face {
        Face::Title => 1.05,
        Face::Chrome | Face::Mono => 1.0,
    };
    ems * size * weight
}

/// `text` as it fits `room`: whole, cut with an ellipsis (never after a
/// space), or nothing when not even the ellipsis fits.
fn fitted(text: &str, room: f32, size: f32, face: Face) -> Option<Cow<'_, str>> {
    if estimate(text, size, face) <= room {
        return Some(Cow::Borrowed(text));
    }
    let ellipsis = estimate("…", size, face);
    if ellipsis > room {
        return None;
    }
    let mut width = ellipsis;
    let mut end = 0;
    for (at, c) in text.char_indices() {
        width += estimate(c.encode_utf8(&mut [0; 4]), size, face);
        if width > room {
            break;
        }
        end = at + c.len_utf8();
    }
    let mut kept = text[..end].trim_end().to_string();
    kept.push(ELLIPSIS);
    Some(Cow::Owned(kept))
}

/// A font-family list: the user's family, quoted, then `fallback`. A family
/// name is the user's to type, so only characters a family name has are kept.
fn family(name: &str, fallback: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .collect();
    match name.trim() {
        "" => fallback.to_string(),
        name => format!("'{name}', {fallback}"),
    }
}

/// Text for an XML element or attribute. Names come from the database and
/// can hold anything, and XML 1.0 has no way to write most control
/// characters even escaped, so those become a replacement character, and
/// line breaks and tabs a space.
pub fn escape(text: &str) -> Cow<'_, str> {
    let clean = |c: char| !matches!(c, '<' | '>' | '&' | '"' | '\'') && !c.is_control();
    if text.chars().all(clean) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(' '),
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

fn hex(colour: Srgb) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(colour.r),
        channel(colour.g),
        channel(colour.b)
    )
}

/// A coordinate to two places, without trailing zeros.
fn num(value: f32) -> String {
    let fixed = format!("{:.2}", value);
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    match trimmed {
        "-0" | "" => "0".to_string(),
        trimmed => trimmed.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::{GraphColumn, GraphKey, GraphTable};
    use crate::diagram::index::{Placement, Scene};
    use crate::diagram::layout::schema;
    use crate::diagram::tests::graph;

    pub(crate) fn style() -> Style {
        let tone = Srgb::from_hex;
        Style {
            palette: Palette {
                canvas: tone(0x101010),
                card: tone(0x202020),
                header: tone(0x303030),
                border: tone(0x404040),
                text: tone(0xf0f0f0),
                muted: tone(0xa0a0a0),
                edge: tone(0x808080),
                primary: tone(0x3399ff),
                foreign: tone(0xcc66ff),
            },
            chrome: "IBM Plex Sans".into(),
            grid: "Lilex".into(),
            name_size: 13.0,
            type_size: 12.0,
        }
    }

    fn count(document: &str, needle: &str) -> usize {
        document.matches(needle).count()
    }

    #[test]
    fn a_file_has_a_card_per_table_a_path_per_key_and_text_for_every_name() {
        let graph = graph(3, &[(1, 0), (2, 1)]);
        let layout = schema(&graph, false);
        let document = svg(&graph, &layout, &style());
        assert!(document.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg "));
        assert!(document.ends_with("</svg>\n"));
        assert_eq!(count(&document, r#"<rect class="card""#), 3);
        assert_eq!(count(&document, r#"<rect class="outline""#), 3);
        assert_eq!(
            count(&document, "<path class=\"key\" d=\"M"),
            2,
            "one path per key"
        );
        assert_eq!(count(&document, r#"<text class="name title""#), 3);
        for name in ["t0", "t1", "t2", "t1_id", "t0_id"] {
            assert!(document.contains(&format!(">{name}</text>")), "{name}");
        }
        assert_eq!(count(&document, r##"xlink:href="#pk""##), 3);
        assert_eq!(count(&document, r##"xlink:href="#fk""##), 2);
        assert_eq!(count(&document, ">bigint</text>"), 5, "a type per column");
        assert!(document.contains("font-family:'IBM Plex Sans', system-ui"));
        assert!(document.contains("font-family:'Lilex', ui-monospace"));
        assert!(document.contains(r##"fill="#101010""##), "the canvas tone");
        let frame = frame(&layout);
        assert!(document.contains(&format!(
            r#"width="{}" height="{}" viewBox="{} {} "#,
            num(frame.width),
            num(frame.height),
            num(frame.x),
            num(frame.y),
        )));
    }

    #[test]
    fn names_are_escaped_and_control_characters_never_reach_the_file() {
        let mut graph = graph(2, &[(1, 0)]);
        graph.schema = "a&b".into();
        graph.tables[0].name = "<script>\"x\" & 'y'".into();
        graph.tables[1].columns[1].name = "bell\u{7}tab\tnul\0".into();
        graph.types[0] = "enum<'a'>".into();
        let layout = schema(&graph, false);
        let document = svg(&graph, &layout, &style());
        assert!(document.contains("<title>a&amp;b</title>"));
        assert!(document.contains(">&lt;script&gt;&quot;x&quot; &amp; &apos;y&apos;</text>"));
        assert!(document.contains(">bell\u{FFFD}tab nul\u{FFFD}</text>"));
        assert!(document.contains(">enum&lt;&apos;a&apos;&gt;</text>"));
        assert!(!document.contains("<script"));
        assert!(
            !document.chars().any(|c| c.is_control() && c != '\n'),
            "no control character but the line breaks between elements"
        );
    }

    #[test]
    fn a_self_reference_draws_a_loop_with_its_marks() {
        let graph = graph(1, &[(0, 0)]);
        let layout = schema(&graph, false);
        let document = svg(&graph, &layout, &style());
        let card = &layout.cards[0];
        let right = num(card.origin.x + card.width);
        let path = document
            .lines()
            .find(|line| line.starts_with("<path class=\"key\" d=\"M"))
            .expect("the loop is drawn");
        assert!(
            path.starts_with(&format!("<path class=\"key\" d=\"M{right} ")),
            "{path}"
        );
        assert_eq!(count(path, "C"), 1, "one curve out and back: {path}");
        assert_eq!(
            count(path, "M"),
            1 + 3 + 2,
            "a crow's foot and two bars: {path}"
        );
    }

    #[test]
    fn a_key_into_another_schema_draws_a_stub_by_its_qualified_name() {
        let mut graph = graph(1, &[]);
        graph.tables[0].columns.push(GraphColumn {
            name: "owner".into(),
            type_label: 0,
            nullable: true,
            primary: false,
            foreign: true,
            unique: false,
        });
        graph.keys.push(GraphKey {
            name: "fk_owner".into(),
            table: 0,
            columns: vec![1],
            referenced: KeyTarget::Elsewhere {
                schema: "auth".into(),
                table: "users".into(),
            },
        });
        let layout = schema(&graph, false);
        let document = svg(&graph, &layout, &style());
        assert_eq!(count(&document, r#"<rect class="card""#), 2);
        assert!(document.contains(">auth.users</text>"));
        assert!(document.contains(">bigint?</text>"), "nullable wears a ?");
        assert_eq!(
            count(&document, r#"<circle class="edge""#),
            1,
            "a nullable key's zero-or-one circle"
        );
    }

    #[test]
    fn a_card_cut_short_ends_with_more_and_moved_cards_are_drawn_where_they_were_left() {
        let mut graph = graph(1, &[]);
        graph.tables[0] = GraphTable {
            name: "wide".into(),
            columns: (0..10)
                .map(|column| GraphColumn {
                    name: format!("c{column}").into(),
                    type_label: 0,
                    nullable: false,
                    primary: column == 0,
                    foreign: false,
                    unique: column == 0,
                })
                .collect(),
        };
        let mut scene = Scene::new(schema(&graph, false));
        scene.place(
            0,
            &Placement {
                origin: Some(super::super::geometry::Point::new(500.0, 700.0)),
                width: Some(300.0),
                height: Some(HEADER_HEIGHT + 4.0 * ROW_HEIGHT + 8.0),
            },
        );
        let document = svg(&graph, scene.layout(), &style());
        assert!(
            document.contains(">+7 more</text>"),
            "three rows and the cut"
        );
        assert_eq!(count(&document, ">bigint</text>"), 3);
        assert!(document.contains(r#"<rect class="card" x="500" y="700" width="300""#));
        assert!(document.contains(r#"viewBox="468 668 364 "#), "{document}");
    }

    #[test]
    fn keys_only_draws_the_key_columns_without_types() {
        let mut graph = graph(2, &[(1, 0)]);
        graph.tables[1].columns.push(GraphColumn {
            name: "payload".into(),
            type_label: 0,
            nullable: true,
            primary: false,
            foreign: false,
            unique: false,
        });
        let layout = schema(&graph, true);
        let document = svg(&graph, &layout, &style());
        assert!(!document.contains(">payload</text>"));
        assert!(document.contains(">t0_id</text>"));
        assert!(
            !document.contains(r#"class="type""#),
            "no types at the keys level"
        );
    }

    #[test]
    fn a_long_name_is_cut_with_an_ellipsis_inside_its_card() {
        let mut graph = graph(1, &[]);
        graph.tables[0].name = "a_table_name_far_too_long_to_fit_on_any_card_at_all".into();
        let layout = schema(&graph, false);
        let document = svg(&graph, &layout, &style());
        let title = document
            .lines()
            .find(|line| line.contains("class=\"name title\""))
            .unwrap();
        assert!(title.ends_with("…</text>"), "{title}");
        assert_eq!(fitted("abc", 1.0, 13.0, Face::Chrome), None);
        assert_eq!(
            fitted(
                "ab cd",
                estimate("ab c", 13.0, Face::Chrome),
                13.0,
                Face::Chrome
            )
            .as_deref(),
            Some("ab…"),
            "never a space before the ellipsis"
        );
    }

    #[test]
    fn numbers_are_short_and_families_hold_only_name_characters() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(-0.001), "0");
        assert_eq!(num(12.345), "12.35");
        assert_eq!(num(-3.5), "-3.5");
        assert_eq!(family("Evil'};<x>", "serif"), "'Evilx', serif");
        assert_eq!(family("  ", "serif"), "serif");
    }
}
