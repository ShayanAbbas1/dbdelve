//! Remote cleanup notices are views of app-lifetime state, not the work's owner.
use super::*;
use crate::db::ssm::cleanup::{Manager, Outcome, Phase};
use gpui_component::Disableable;

#[derive(Default)]
pub(super) struct SharedCleanup(pub Manager);
impl gpui::Global for SharedCleanup {}

impl Workspace {
    #[cfg(test)]
    pub(super) fn cleanup_notices(&self) -> Vec<crate::db::ssm::CleanupResult> {
        self.cleanup
            .records()
            .into_iter()
            .filter(|r| !r.dismissed)
            .map(|r| r.result)
            .collect()
    }
    pub(crate) fn pending_cleanup_count(&self) -> usize {
        self.cleanup
            .records()
            .iter()
            .filter(|r| r.phase != Phase::Confirmed)
            .count()
    }
    pub(super) fn show_pending_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cleanup.reopen();
        self.focus.focus(window, cx);
        self.refocus_front();
        cx.notify();
    }
    fn dismiss_cleanup_notice(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.cleanup.dismiss(id);
        self.focus.focus(window, cx);
        self.refocus_front();
        cx.notify();
    }
    pub(super) fn render_cleanup_notice(
        &self,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let records = self.cleanup.records();
        let pending = records
            .iter()
            .filter(|r| r.phase != Phase::Confirmed)
            .count();
        let Some(record) = records
            .iter()
            .filter(|r| !r.dismissed)
            .min_by_key(|r| r.phase == Phase::Confirmed)
        else {
            return (pending > 0).then(|| {
                div()
                    .flex()
                    .items_start()
                    .flex_shrink_0()
                    .px(px(layout::SPACE_MD))
                    .py(px(layout::SPACE_XS))
                    .child(
                        cleanup_button(
                            "cleanup-pending",
                            format!("Pending AWS cleanup ({pending})"),
                            true,
                            t,
                        )
                        .debug_selector(|| "cleanup-pending".into())
                        .on_click(cx.listener(
                            |workspace, _, window, cx| workspace.show_pending_cleanup(window, cx),
                        )),
                    )
            });
        };
        let id = record.id;
        let result = &record.result;
        let profile = result.profile.as_deref().map_or_else(
            || "Profile: AWS CLI configuration".to_string(),
            |profile| format!("Configured profile: {profile}"),
        );
        let profile_selector = format!("cleanup-profile: {profile}");
        let region = result.region.as_deref().unwrap_or("AWS CLI configuration");
        let status = match result.outcome {
            Outcome::Confirmed => "AWS session termination confirmed.",
            Outcome::AuthenticationRequired => {
                "AWS sign-in required. Renew your CLI credentials, then retry cleanup."
            }
            Outcome::PermissionDenied => {
                "Permission denied. Check ssm:TerminateSession access, then retry cleanup."
            }
            Outcome::Transient => {
                "AWS session termination could not be confirmed due to a network or temporary service failure."
            }
            Outcome::MissingId => {
                "Session ID unavailable. Check your sessions in AWS; automatic cleanup is not possible."
            }
            Outcome::Unconfirmed => {
                "AWS session termination could not be confirmed. Check your CLI configuration or the session in AWS."
            }
        };
        let progress = match record.phase {
            Phase::Scheduled => "Retry scheduled. Local tunnel is closed.",
            Phase::Running => "Retrying cleanup… Local tunnel is closed.",
            Phase::Paused => "Automatic retries paused.",
            Phase::Exhausted => "Automatic retry limit reached. You can retry when ready.",
            Phase::Stopped => "Retries stopped. The AWS session may still be active.",
            Phase::Confirmed => "The remote session is closed.",
        };
        let busy = matches!(record.phase, Phase::Scheduled | Phase::Running);
        let remaining = records
            .iter()
            .filter(|r| !r.dismissed)
            .count()
            .saturating_sub(1);
        Some(div().flex_shrink_0().w_full().px(px(layout::SPACE_MD)).py(px(layout::SPACE_SM))
            .border_b_1().border_color(t.border).flex().flex_col().gap(px(layout::SPACE_XS))
            .text_size(px(layout::chrome(layout::TEXT_SM))).text_color(t.text_muted)
            .child(div().debug_selector(|| "cleanup-status".into()).child(status))
            .child(div().debug_selector(|| "cleanup-progress".into()).child(progress))
            .child(div().debug_selector(move || profile_selector.clone()).child(format!("{profile} · Region: {region} (account and effective region unverified)")))
            .child(result.session.as_ref().map_or_else(|| "Session ID unavailable".to_string(), |id| format!("Session ID: {id}")))
            .child(div().flex().flex_wrap().items_center().gap(px(layout::SPACE_SM))
                .children(result.session.clone().map(|session| cleanup_button("cleanup-copy", "Copy session ID", true, t)
                    .debug_selector(|| "cleanup-copy-0".into())
                    .on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(session.clone()))))))
                .child(cleanup_button("cleanup-retry", "Retry", record.can_retry, t)
                    .debug_selector(|| "cleanup-retry".into())
                    .on_click(cx.listener(move |workspace, _, _, cx| { workspace.cleanup.retry(id); cx.notify(); })))
                .child(cleanup_button("cleanup-stop", "Stop retrying", busy, t)
                    .debug_selector(|| "cleanup-stop".into())
                    .on_click(cx.listener(move |workspace, _, _, cx| { workspace.cleanup.stop(id); cx.notify(); })))
                .child(cleanup_button("cleanup-dismiss", "Dismiss", true, t)
                    .debug_selector(|| "cleanup-dismiss-0".into())
                    .on_click(cx.listener(move |workspace, _, window, cx| workspace.dismiss_cleanup_notice(id, window, cx))))
                .when(remaining > 0, |row| row.child(format!("{remaining} more"))))
            .child("Dismiss hides this notice; pending cleanup stays available while DBDelve is open."))
    }
}
fn cleanup_button(
    id: &'static str,
    label: impl Into<gpui::SharedString>,
    enabled: bool,
    t: Theme,
) -> gpui_component::button::Button {
    ui::button(
        id,
        label,
        if enabled { Tone::Primary } else { Tone::Quiet },
        Control::Compact,
        t,
    )
    .disabled(!enabled)
    .when(!enabled, |button| button.opacity(0.5))
    .border_1()
    .border_color(t.border)
    .bg(t.element_active)
    .rounded(px(layout::RADIUS_CONTROL))
}
