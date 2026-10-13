//! Conversation rewind dispatchers and prompt-entry lookup helpers.

use super::ctx::NO_SESSION_NOTICE;
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::state::ScrollbackState;
use crate::views::prompt_widget::{PromptWidget, StashedPrompt};
use crate::views::rewind::{RewindPhase, RewindState};

/// Interjections render as user prompts but the shell never numbers them, so counting them would skew the positional prompt-to-entry mapping.
/// Known approximation: an interjection the shell converted into its own `interject-fallback-` turn IS shell-numbered.
/// The positional fallback thus under-counts around it until a resume replays it as an indexed prompt.
fn is_indexed_user_prompt(block: &RenderBlock) -> bool {
    matches!(block, RenderBlock::UserPrompt(b) if !b.is_interjection)
}

fn stash_prompt(prompt: &mut PromptWidget) -> Option<StashedPrompt> {
    if prompt.text().is_empty() {
        None
    } else {
        Some(prompt.stash())
    }
}

pub(in crate::app) fn shell_prompt_index_at(
    scrollback: &ScrollbackState,
    entry_idx: usize,
) -> Option<usize> {
    for idx in (0..=entry_idx).rev() {
        if let Some(e) = scrollback.get(idx)
            && let RenderBlock::UserPrompt(ref block) = e.block
        {
            // A mid-turn interjection belongs to the enclosing turn; keep walking back to that turn's starting prompt
            if block.is_interjection {
                continue;
            }
            if let Some(pi) = block.prompt_index {
                return Some(pi);
            }
            let count = (0..=idx)
                .filter(|&i| {
                    scrollback
                        .get(i)
                        .is_some_and(|e2| is_indexed_user_prompt(&e2.block))
                })
                .count();
            return if count > 0 { Some(count - 1) } else { None };
        }
    }
    None
}

pub(in crate::app) fn find_user_prompt_entry_for_shell_index(
    scrollback: &ScrollbackState,
    target_prompt_index: usize,
) -> Option<usize> {
    for idx in (0..scrollback.len()).rev() {
        if let Some(entry) = scrollback.get(idx)
            && let RenderBlock::UserPrompt(ref block) = entry.block
            && block.prompt_index == Some(target_prompt_index)
        {
            return Some(idx);
        }
    }
    let mut count = 0usize;
    for idx in 0..scrollback.len() {
        if let Some(e) = scrollback.get(idx)
            && is_indexed_user_prompt(&e.block)
        {
            if count == target_prompt_index {
                return Some(idx);
            }
            count += 1;
        }
    }
    None
}

pub(super) fn dispatch_rewind(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        app.show_toast(NO_SESSION_NOTICE);
        return vec![];
    };

    // Rewind takes input priority over the `/jump` picker; close a lingering one first so it can't reappear (stale) after rewind finishes
    agent.dismiss_jump_picker();

    let selected_idx = agent.scrollback.selected();
    let selected_shell_idx =
        selected_idx.and_then(|idx| shell_prompt_index_at(&agent.scrollback, idx));

    if agent.session.state.is_busy() {
        let anchor = agent.scrollback.len().saturating_sub(1);
        let draft = stash_prompt(&mut agent.prompt);
        agent.rewind_state = Some(RewindState::new_cancel_offer(
            anchor,
            draft,
            selected_shell_idx,
            false,
        ));
        return vec![];
    }

    let draft = stash_prompt(&mut agent.prompt);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Loading,
        anchor_entry_idx: selected_idx.unwrap_or(0),
        stashed_draft: draft,
        // Cursor-derived: the rewind keybinding is not bound to the
        // cursor position. `handle_rewind_points_loaded` resolves the
        // target to the LATEST prompt for this path, so a scrolled-up
        // cursor (or a retired last-prompt echo) cannot drag the
        // target back into earlier turns.
        selected_prompt_index: selected_shell_idx,
        selected_prompt_is_explicit: false,
    });

    vec![Effect::FetchRewindPoints {
        agent_id: id,
        session_id,
    }]
}

pub(super) fn dispatch_rewind_show_picker(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        app.show_toast(NO_SESSION_NOTICE);
        return vec![];
    };

    // Rewind takes input priority over the `/jump` picker; close a lingering one first so it can't reappear (stale) after rewind finishes
    agent.dismiss_jump_picker();

    if agent.session.state.is_busy() {
        let anchor = agent.scrollback.len().saturating_sub(1);
        let draft = stash_prompt(&mut agent.prompt);
        agent.rewind_state = Some(RewindState::new_cancel_offer(anchor, draft, None, false));
        return vec![];
    }

    let draft = stash_prompt(&mut agent.prompt);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Loading,
        anchor_entry_idx: 0,
        stashed_draft: draft,
        selected_prompt_index: None,
        selected_prompt_is_explicit: false,
    });

    vec![Effect::FetchRewindPoints {
        agent_id: id,
        session_id,
    }]
}

pub(super) fn dispatch_rewind_picker_select(app: &mut AppView, prompt_index: usize) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let confirm = app.current_ui.confirm_before_rewind_enabled();
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    let point = agent.rewind_points.as_ref().and_then(
        |pts: &Vec<crate::views::rewind::RewindPointInfo>| {
            pts.iter().find(|p| p.prompt_index == prompt_index)
        },
    );
    let preview = point.and_then(|p| p.prompt_preview.clone());

    let anchor = find_user_prompt_entry_for_shell_index(&agent.scrollback, prompt_index);
    if let Some(entry_idx) = anchor {
        agent.scrollback.set_selected(Some(entry_idx));
    }

    let draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);
    begin_rewind(
        agent,
        id,
        prompt_index,
        anchor.unwrap_or(0),
        draft,
        preview,
        confirm,
    )
}

pub(super) fn dispatch_rewind_cancel_offer(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    let anchor = agent
        .rewind_state
        .as_ref()
        .map(|s| s.anchor_entry_idx)
        .unwrap_or(0);
    let selected = agent
        .rewind_state
        .as_ref()
        .and_then(|s| s.selected_prompt_index);
    let selected_is_explicit = agent
        .rewind_state
        .as_ref()
        .map(|s| s.selected_prompt_is_explicit)
        .unwrap_or(false);
    let draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Loading,
        anchor_entry_idx: anchor,
        stashed_draft: draft,
        selected_prompt_index: selected,
        selected_prompt_is_explicit: selected_is_explicit,
    });
    vec![Effect::CancelTurnThenFetchRewindPoints {
        agent_id: id,
        session_id,
    }]
}

pub(super) fn dispatch_rewind_confirm(app: &mut AppView, target: usize) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let anchor = agent
        .rewind_state
        .as_ref()
        .map(|s| s.anchor_entry_idx)
        .unwrap_or(0);
    let draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);
    enter_executing(agent, id, target, anchor, draft)
}

/// "Yes, and don't ask again": quiet-persist confirm-before-rewind off, then execute.
/// No settings checkmark toast; success/toast comes from the rewind itself.
pub(super) fn dispatch_rewind_confirm_never_ask(app: &mut AppView, target: usize) -> Vec<Effect> {
    let mut effects = Vec::new();
    let prev = app.current_ui.confirm_before_rewind_enabled();
    if prev {
        super::settings::setters::set_confirm_before_rewind_inner(app, false);
        super::settings::ui::refresh_open_settings_modals(app);
        effects.push(Effect::PersistSetting {
            key: "confirm_before_rewind",
            value: crate::settings::SettingValue::Bool(false),
            rollback_value: crate::settings::SettingValue::Bool(true),
        });
    }
    effects.extend(dispatch_rewind_confirm(app, target));
    effects
}

pub(super) fn dispatch_rewind_dismiss(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);
    if let Some(d) = draft {
        agent.prompt.restore(d);
    }
    agent.rewind_points = None;
    vec![]
}

pub(super) fn dispatch_rewind_dismiss_error(app: &mut AppView) -> Vec<Effect> {
    dispatch_rewind_dismiss(app)
}

/// Enter `Executing` and emit `RewindExecute` (shared by confirm Yes and immediate execute when confirm-before-rewind is off).
fn enter_executing(
    agent: &mut crate::app::agent_view::AgentView,
    agent_id: AgentId,
    target: usize,
    anchor: usize,
    draft: Option<StashedPrompt>,
) -> Vec<Effect> {
    let Some(session_id) = agent.session.session_id.clone() else {
        if let Some(d) = draft {
            agent.prompt.restore(d);
        }
        agent.rewind_state = None;
        agent.rewind_points = None;
        return vec![];
    };
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Executing {
            target_prompt_index: target,
        },
        anchor_entry_idx: anchor,
        stashed_draft: draft,
        selected_prompt_index: None,
        selected_prompt_is_explicit: false,
    });
    vec![Effect::RewindExecute {
        agent_id,
        session_id,
        target_prompt_index: target,
    }]
}

/// When `confirm` is true, open the confirm dialog for any target; otherwise execute immediately.
fn begin_rewind(
    agent: &mut crate::app::agent_view::AgentView,
    agent_id: AgentId,
    target: usize,
    anchor: usize,
    draft: Option<StashedPrompt>,
    prompt_preview: Option<String>,
    confirm: bool,
) -> Vec<Effect> {
    if confirm {
        agent.rewind_state = Some(RewindState {
            phase: RewindPhase::Confirm {
                target_prompt_index: target,
                active_idx: 0,
                prompt_preview,
            },
            anchor_entry_idx: anchor,
            stashed_draft: draft,
            selected_prompt_index: Some(target),
            selected_prompt_is_explicit: true,
        });
        return vec![];
    }
    enter_executing(agent, agent_id, target, anchor, draft)
}

pub(super) fn dispatch_rewind_success(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    response: crate::views::rewind::RewindResponse,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };

    if !response.success {
        let err = response.error.unwrap_or_else(|| "unknown error".into());
        let anchor = agent
            .rewind_state
            .as_ref()
            .map(|s| s.anchor_entry_idx)
            .unwrap_or(0);
        let draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);
        agent.rewind_state = Some(RewindState {
            phase: RewindPhase::Error { message: err },
            anchor_entry_idx: anchor,
            stashed_draft: draft,
            selected_prompt_index: None,
            selected_prompt_is_explicit: false,
        });
        return vec![];
    }

    let target = response.target_prompt_index;
    let stashed_draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);

    // The summary describes turns the rewind just removed (the shell clears its persisted copy on the same branch)
    // Bump gen so a late SessionMetaFromDisk hydrate cannot restore the pre-rewind summary.json value into the cleared field
    agent.set_last_turn_summary(None);
    let target_idx = find_user_prompt_entry_for_shell_index(&agent.scrollback, target);
    if let Some(anchor_idx) = target_idx {
        let removed = agent.scrollback.remove_from(anchor_idx);
        // Explicit drop BEFORE the purge: the rewound tail must be freed for the release below to return its pages
        // (Entries and their render caches are potentially most of a long transcript.)
        drop(removed);
        crate::memory_release::release_retained_memory("rewind-truncate");
    }

    const MSG: &str = "Reverted conversation";
    if app.screen_mode.is_minimal() {
        // Minimal has no toast area and can't erase committed lines, so the confirmation stays in scrollback there
        agent
            .scrollback
            .push_block(RenderBlock::system(MSG.to_string()));
    } else {
        agent.show_toast(MSG);
    }

    if let Some(ref prompt_text) = response.prompt_text {
        agent.prompt.set_text_discarding_images(prompt_text);
    } else if let Some(draft) = stashed_draft {
        agent.prompt.restore(draft);
    }

    agent.set_active_pane(crate::app::agent_view::ActivePane::Prompt, false);

    agent.rewind_points = None;
    agent.scrollback.goto_bottom();

    vec![]
}

// TaskResult handlers.

pub(super) fn handle_rewind_points_loaded(
    app: &mut AppView,
    agent_id: AgentId,
    points: Vec<crate::views::rewind::RewindPointInfo>,
) -> Vec<Effect> {
    let confirm = app.current_ui.confirm_before_rewind_enabled();
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    agent.rewind_points = Some(points.clone());

    // Resolve the intended target. An inline-edit submit names its
    // prompt explicitly and must hit exactly that prompt. The rewind
    // KEYBINDING is not cursor-bound: it always undoes the LATEST
    // prompt. Trusting the cursor-derived index here is the bug-1
    // failure mode — a cursor parked on earlier rounds (the user
    // scrolled up to inspect them), an interjection, or a last
    // prompt whose echo was retired after a send failure, all drag
    // the target back into earlier turns and wipe their tool calls
    // (ctx collapses to the pre-turn size). A `None` target is the
    // `/rewind` picker flow: the picker decides.
    let desired_target = match agent.rewind_state.as_ref() {
        Some(s) if s.selected_prompt_is_explicit => s.selected_prompt_index,
        Some(s) if s.selected_prompt_index.is_some() => {
            points.iter().map(|p| p.prompt_index).max()
        }
        _ => None,
    };
    let stashed = agent.rewind_state.take().and_then(|s| s.stashed_draft);

    if points.is_empty() {
        if let Some(stashed) = stashed {
            agent.prompt.restore(stashed);
        }
        app.show_toast("No undoable prompts");
        return vec![];
    }

    if let Some(dt) = desired_target {
        let resolved = points
            .iter()
            .find(|p| p.prompt_index == dt)
            .or_else(|| points.iter().max_by_key(|p| p.prompt_index))
            .cloned();

        if let Some(point) = resolved {
            let target = point.prompt_index;
            let preview = point.prompt_preview.clone();
            let anchor = find_user_prompt_entry_for_shell_index(&agent.scrollback, target);
            let draft = stashed.or_else(|| stash_prompt(&mut agent.prompt));
            if let Some(entry_idx) = anchor {
                agent.scrollback.set_selected(Some(entry_idx));
            }
            return begin_rewind(
                agent,
                agent_id,
                target,
                anchor.unwrap_or(0),
                draft,
                preview,
                confirm,
            );
        }
    }

    let mut sorted = points;
    sorted.sort_by(|a, b| b.prompt_index.cmp(&a.prompt_index));
    let draft = stashed.or_else(|| stash_prompt(&mut agent.prompt));
    let initial_anchor = sorted
        .first()
        .map(|p| {
            find_user_prompt_entry_for_shell_index(&agent.scrollback, p.prompt_index).unwrap_or(0)
        })
        .unwrap_or(0);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Picker {
            points: sorted,
            selected: 0,
        },
        anchor_entry_idx: initial_anchor,
        stashed_draft: draft,
        selected_prompt_index: None,
        selected_prompt_is_explicit: false,
    });
    agent.scrollback.scroll_to_entry_center(initial_anchor);
    vec![]
}

pub(super) fn handle_rewind_execute_failed(
    app: &mut AppView,
    agent_id: AgentId,
    error: String,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    let anchor = agent
        .rewind_state
        .as_ref()
        .map(|s| s.anchor_entry_idx)
        .unwrap_or(0);
    let draft = agent.rewind_state.take().and_then(|s| s.stashed_draft);
    agent.rewind_state = Some(RewindState {
        phase: RewindPhase::Error { message: error },
        anchor_entry_idx: anchor,
        stashed_draft: draft,
        selected_prompt_index: None,
        selected_prompt_is_explicit: false,
    });
    vec![]
}

#[cfg(test)]
mod rewind_target_repro {
    use super::*;
    use crate::scrollback::blocks::UserPromptBlock;

    fn user_prompt_pi(text: &str, pi: usize) -> RenderBlock {
        let mut b = UserPromptBlock::new(text);
        b.prompt_index = Some(pi);
        RenderBlock::UserPrompt(b)
    }

    fn interjection(text: &str) -> RenderBlock {
        let mut b = UserPromptBlock::new(text);
        b.is_interjection = true;
        RenderBlock::UserPrompt(b)
    }

    fn round(i: usize) -> RenderBlock {
        RenderBlock::agent_message(format!("round {i}"))
    }

    enum ContinueKind {
        /// 继续 sent after prompt0's turn had already ended.
        Direct,
        /// 继续 sent while prompt0's turn was still stuck on the
        /// network (a mid-turn steer).
        Interjection,
        /// 继续's optimistic echo was retired on the send failure.
        Absent,
    }

    /// The user's bug-1 scrollback: prompt0 ("do a large task")
    /// followed by 50 tool-call rounds, then a failed "继续"
    /// rendered per `continue_kind`.
    fn scenario(continue_kind: ContinueKind) -> ScrollbackState {
        let mut sb = ScrollbackState::new();
        sb.push_block(user_prompt_pi("do a large task", 0));
        for i in 0..50 {
            sb.push_block(round(i));
        }
        match continue_kind {
            ContinueKind::Direct => {
                sb.push_block(user_prompt_pi("继续", 1));
            }
            ContinueKind::Interjection => {
                sb.push_block(interjection("继续"));
            }
            ContinueKind::Absent => {}
        }
        sb
    }

    /// Control: cursor on the failed "继续" (a real direct prompt,
    /// present at the bottom) resolves to prompt 1 — only the
    /// failed turn is rewound, prompt0's 50 rounds survive.
    #[test]
    fn control_cursor_on_direct_continue_is_prompt1() {
        let sb = scenario(ContinueKind::Direct);
        let bottom = sb.len() - 1;
        assert_eq!(shell_prompt_index_at(&sb, bottom), Some(1));
    }

    /// Reproduction (bug 1): "继续" was sent as an interjection
    /// while prompt0's turn was still stuck. An interjection
    /// carries no prompt_index and is skipped by
    /// `shell_prompt_index_at`, so the bottom cursor resolves to
    /// prompt 0. The rewind KEYBINDING no longer trusts this
    /// value (`handle_rewind_points_loaded` targets the latest
    /// prompt for that path); the picker and the inline-edit
    /// flow still rely on the helper.
    #[test]
    fn repro_interjection_continue_resolves_to_prompt0() {
        let sb = scenario(ContinueKind::Interjection);
        let bottom = sb.len() - 1;
        assert_eq!(
            shell_prompt_index_at(&sb, bottom),
            Some(0),
            "cursor-derived lookup skips the interjection and falls back to prompt 0; the keybinding path must not trust it"
        );
    }

    /// Reproduction (bug 1): the failed "继续" block is absent
    /// (its optimistic echo was retired on the send failure). The
    /// bottom cursor falls back to prompt0's last round. The
    /// rewind keybinding must not trust that fallback (see
    /// `handle_rewind_points_loaded`).
    #[test]
    fn repro_absent_continue_falls_back_to_prompt0() {
        let sb = scenario(ContinueKind::Absent);
        let bottom = sb.len() - 1;
        assert_eq!(
            shell_prompt_index_at(&sb, bottom),
            Some(0),
            "cursor-derived lookup falls back to prompt 0; the keybinding path must not trust it"
        );
    }

    /// Reproduction (bug 1): the user scrolled up to inspect
    /// prompt0's 50 rounds (follow mode off). The cursor sits on
    /// a prompt0 round, so the cursor-derived lookup resolves to
    /// prompt 0. The rewind keybinding must not trust it (see
    /// `handle_rewind_points_loaded`).
    #[test]
    fn repro_cursor_on_prior_rounds_resolves_to_prompt0() {
        let sb = scenario(ContinueKind::Direct);
        let on_prompt0_round = 1;
        assert_eq!(
            shell_prompt_index_at(&sb, on_prompt0_round),
            Some(0),
            "cursor-derived lookup resolves to prompt 0; the keybinding path must not trust it"
        );
    }
}
