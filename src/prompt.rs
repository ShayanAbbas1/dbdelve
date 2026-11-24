//! The window's prompt where the platform has none. GPUI's own stand-in is an
//! unthemed card that answers only to the mouse and lets clicks through to
//! the window behind it.

use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, PromptButton, PromptHandle, PromptLevel,
    PromptResponse, Render, RenderablePromptHandle, SharedString, Styled, Window, div, px,
};
use gpui_component::ActiveTheme;

use crate::{
    theme::{layout, popup_theme},
    ui::{Control, Tone, button, dialog},
};

/// Only off macOS and Windows: a builder, once set, replaces their native
/// dialog too.
pub(crate) fn install(cx: &mut App) {
    if cfg!(not(any(target_os = "macos", target_os = "windows"))) {
        cx.set_prompt_builder(build);
    }
}

pub(crate) fn build(
    _: PromptLevel,
    message: &str,
    _: Option<&str>,
    answers: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let prompt = cx.new(|cx| Prompt {
        message: SharedString::from(message.to_string()),
        answers: answers.to_vec(),
        focus: cx.focus_handle(),
    });
    // Takes the keyboard, and hands it back to whatever had it once answered.
    handle.with_view(prompt, window, cx)
}

struct Prompt {
    message: SharedString,
    answers: Vec<PromptButton>,
    focus: FocusHandle,
}

impl EventEmitter<PromptResponse> for Prompt {}

impl Focusable for Prompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Prompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = popup_theme(cx);
        let buttons = self.answers.iter().enumerate().map(|(at, answer)| {
            let tone = match answer {
                PromptButton::Ok(_) => Tone::Primary,
                _ => Tone::Quiet,
            };
            let label = answer.label().clone();
            div()
                .debug_selector(move || format!("prompt: {label}"))
                .child(
                    button(
                        ("prompt", at),
                        answer.label().clone(),
                        tone,
                        Control::Standard,
                        t,
                    )
                    .w_full()
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(PromptResponse(at)))),
                )
        });
        // Drawn by the window over its root, so outside `Root` and every
        // style and key context it sets.
        div()
            .size_full()
            .occlude()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|prompt, event: &KeyDownEvent, _, cx| {
                let answer = match event.keystroke.key.as_str() {
                    "enter" => prompt
                        .answers
                        .iter()
                        .position(|answer| matches!(answer, PromptButton::Ok(_))),
                    "escape" => prompt.answers.iter().position(PromptButton::is_cancel),
                    _ => None,
                };
                if let Some(answer) = answer {
                    cx.stop_propagation();
                    cx.emit(PromptResponse(answer));
                }
            }))
            .font_family(cx.theme().font_family.clone())
            .flex()
            .items_center()
            .justify_center()
            .child(
                dialog(t)
                    .child(
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_MD)))
                            .text_color(t.text)
                            .child(self.message.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_SM))
                            .children(buttons),
                    ),
            )
    }
}
