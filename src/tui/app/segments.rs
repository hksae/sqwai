//! Chat segments: the rows the transcript is built from, and the render-cache
//! bookkeeping that keeps them honest.
//!
//! `segments` and `seg_meta` are always the same length; every mutation goes
//! through the helpers here, so render-cache keys and click targets stay bound
//! to stable segment ids instead of shifting positions.

#![allow(unused_imports)]
use super::*;

impl App {
    pub(super) fn alloc_seg_id(&mut self) -> u64 {
        let id = self.next_seg_id;
        self.next_seg_id += 1;
        id
    }

    /// Append a segment; returns its index. The render cache entry is created
    /// lazily on the next rebuild — nothing already cached is touched.
    pub(super) fn push_segment(&mut self, seg: Segment) -> usize {
        let id = self.alloc_seg_id();
        self.segments.push(seg);
        self.seg_meta.push(SegMeta { id, rev: 0 });
        self.segments.len() - 1
    }

    pub(super) fn insert_segment(&mut self, pos: usize, seg: Segment) {
        let id = self.alloc_seg_id();
        let pos = pos.min(self.segments.len());
        self.segments.insert(pos, seg);
        self.seg_meta.insert(pos, SegMeta { id, rev: 0 });
        // cached rows bake in the positional row tag, so every segment whose
        // index shifted re-renders with its new tag
        for m in self.seg_meta.iter_mut().skip(pos + 1) {
            m.rev += 1;
        }
    }

    pub(super) fn remove_segment(&mut self, i: usize) {
        if i < self.segments.len() {
            self.segments.remove(i);
            self.seg_meta.remove(i);
            // see insert_segment: the shifted tail must repaint its tags
            for m in self.seg_meta.iter_mut().skip(i) {
                m.rev += 1;
            }
        }
    }

    /// Whole-content replacement: logically a new segment, so a fresh id.
    pub(super) fn set_segment(&mut self, i: usize, seg: Segment) {
        if i < self.segments.len() {
            let id = self.alloc_seg_id();
            self.segments[i] = seg;
            self.seg_meta[i] = SegMeta { id, rev: 0 };
        }
    }

    pub(super) fn clear_segments(&mut self) {
        self.segments.clear();
        self.seg_meta.clear();
    }

    /// Retain segments by predicate, keeping identity metadata aligned.
    /// Dropped ids are never reused, so cache entries and click targets that
    /// still reference them simply miss instead of hitting new content.
    /// Survivors whose index shifted repaint (cached rows carry the old
    /// positional tag); survivors that kept their index are untouched.
    pub(super) fn retain_segments(&mut self, mut pred: impl FnMut(&Segment) -> bool) {
        let mut kept_segs = Vec::with_capacity(self.segments.len());
        let mut kept_meta = Vec::with_capacity(self.seg_meta.len());
        for (old_i, (seg, mut meta)) in self
            .segments
            .drain(..)
            .zip(self.seg_meta.drain(..))
            .enumerate()
        {
            if pred(&seg) {
                if kept_segs.len() != old_i {
                    meta.rev += 1;
                }
                kept_segs.push(seg);
                kept_meta.push(meta);
            }
        }
        self.segments = kept_segs;
        self.seg_meta = kept_meta;
    }

    /// Bump the revision of one subagent-chat row (see `touch_segment`).
    pub(super) fn sub_touch(&mut self, id: u64, pos: usize) {
        if let Some(meta) = self
            .subagent_meta
            .get_mut(&id)
            .and_then(|meta| meta.get_mut(pos))
        {
            meta.rev += 1;
        }
    }

    /// Bump every row of one subagent transcript. Subagent chats are short
    /// and viewed on demand; coarse invalidation here keeps the main
    /// transcript's cache precise without borrow puzzles at each call site.
    pub(super) fn sub_touch_all(&mut self, id: u64) {
        if let Some(meta) = self.subagent_meta.get_mut(&id) {
            for m in meta.iter_mut() {
                m.rev += 1;
            }
        }
    }

    /// Insert into a subagent transcript, keeping identity aligned.
    /// The shifted tail repaints (cached rows carry the old positional tag).
    pub(super) fn sub_insert(&mut self, id: u64, pos: usize, seg: Segment) {
        let nid = self.alloc_seg_id();
        if let Some(chat) = self.subagent_chats.get_mut(&id) {
            let pos = pos.min(chat.len());
            chat.insert(pos, seg);
            if let Some(meta) = self.subagent_meta.get_mut(&id) {
                meta.insert(pos, SegMeta { id: nid, rev: 0 });
                for m in meta.iter_mut().skip(pos + 1) {
                    m.rev += 1;
                }
            }
        }
    }

    /// Mark a segment's content changed after in-place mutation through
    /// `segments.get_mut`. Call at every site that writes through the
    /// borrow — an unchanged rev with changed content paints stale rows.
    pub(super) fn touch_segment(&mut self, i: usize) {
        if let Some(meta) = self.seg_meta.get_mut(i) {
            meta.rev += 1;
        }
    }

    pub(super) fn index_of_seg(&self, id: u64) -> Option<usize> {
        self.seg_meta.iter().position(|meta| meta.id == id)
    }

    /// A thought row rebuilt from a saved session: folded and finished, with
    /// the clock the save carried. Saves from before durations existed say 0,
    /// and a 0 renders as a bare `thought` rather than an invented one.
    pub(super) fn restored_thinking(text: &str, ms: u64) -> Segment {
        Segment::Thinking {
            text: text.to_string(),
            expanded: false,
            started: None,
            duration_ms: ms,
            live: false,
        }
    }

    /// Render persisted messages as chat segments (used on start and on resume).
    pub(super) fn load_history_segments(&mut self) {
        // Tool calls and their results are part of the durable provider
        // transcript. Keep the rendered row keyed by call id so batched calls
        // and providers that return results out of order are restored safely.
        let mut pending_tools = std::collections::HashMap::<String, usize>::new();
        // A delegated call has no generic tool row (see handle_tool_start):
        // its one row is the child-chat row, and the child's transcript is
        // never persisted. Rebuild that row from the call + its result so a
        // reloaded session matches what the live path painted.
        let mut pending_children = std::collections::HashMap::<String, usize>::new();
        let mut restored_child_id = 0u64;
        // Anchored mode: summaries recorded the user message that started
        // their turn, so each can be re-attached to the right group even when
        // turns were stopped/failed. Legacy saves (all anchors None) fall
        // back to the old sequential order.
        let anchored_mode = self.session.activity.iter().any(|a| a.user_index.is_some());
        let mut remaining_summaries = self.session.activity.clone();
        let turn_notes = self.session.turn_notes.clone();
        let messages = self.session.messages.clone();
        let mut work_start: Option<usize> = None;
        let mut previous_user: Option<usize> = None;
        let mut turn_user: Option<usize> = None;
        for (message_index, m) in messages.iter().enumerate() {
            match m.role {
                Role::User => {
                    // A stopped/failed turn has no Assistant message to give
                    // it a place in the provider transcript. Restore its note
                    // immediately before the next user turn (or after the loop
                    // for the final turn) to preserve chat order.
                    if let Some(user_index) = previous_user {
                        self.restore_turn_notes(&turn_notes, user_index);
                    }
                    // the dangling work run of that turn also ends here: close
                    // it so the stopped turn does not swallow this user
                    // message (and everything after it) into one giant group
                    if let Some(seg_start) = work_start.take() {
                        self.close_restored_group(
                            seg_start,
                            turn_user,
                            &mut remaining_summaries,
                            anchored_mode,
                        );
                    }
                    previous_user = Some(message_index);
                    turn_user = Some(message_index);
                    self.push_segment(Segment::User(m.content.clone()));
                }
                Role::Assistant if m.tool_calls.is_empty() => {
                    // a restored thought row joins the work run it preceded,
                    // exactly where the live path painted it
                    if !m.thinking.is_empty() {
                        self.push_segment(Self::restored_thinking(&m.thinking, m.thinking_ms));
                    }
                    if let Some(seg_start) = work_start.take() {
                        // A saved session is never streaming: historical work
                        // must begin folded, even if it ended with an error.
                        self.close_restored_group(
                            seg_start,
                            turn_user,
                            &mut remaining_summaries,
                            anchored_mode,
                        );
                    }
                    self.push_segment(Segment::Assistant {
                        text: m.content.clone(),
                        live: false,
                    });
                }
                Role::Assistant => {
                    work_start.get_or_insert(self.segments.len());
                    if !m.thinking.is_empty() {
                        self.push_segment(Self::restored_thinking(&m.thinking, m.thinking_ms));
                    }
                    let trimmed = m.content.trim();
                    if !trimmed.is_empty() {
                        self.push_segment(Segment::Commentary(trimmed.to_string()));
                    }
                    for call in &m.tool_calls {
                        if call.name == "subagent" {
                            restored_child_id += 1;
                            let idx = self.push_segment(Segment::Subagent {
                                id: restored_child_id,
                                task: crate::agent::tools::call_summary(&call.name, &call.args),
                                // refined by the matching tool result below;
                                // an interrupted delegation keeps the marker
                                // (its row is the only trace it existed)
                                status: "running".into(),
                                output: String::new(),
                                expanded: false,
                            });
                            pending_children.insert(call.id.clone(), idx);
                            continue;
                        }
                        let idx = self.push_segment(Segment::Tool {
                            name: call.name.clone(),
                            args: crate::agent::tools::call_summary(&call.name, &call.args),
                            call_id: Some(call.id.clone()),
                            ok: None,
                            output: String::new(),
                            diff: None,
                            preview: Vec::new(),
                            preview_total: 0,
                            expanded: false,
                            flash: None,
                            ask: None,
                        });
                        pending_tools.insert(call.id.clone(), idx);
                    }
                }
                Role::Tool => {
                    if let Some(call_id) = m.tool_call_id.as_ref()
                        && let Some(idx) = pending_children.remove(call_id)
                    {
                        if let Some(Segment::Subagent { status, output, .. }) =
                            self.segments.get_mut(idx)
                        {
                            *status = if m.is_error { "failed" } else { "completed" }.into();
                            *output = m.content.clone();
                        }
                        self.touch_segment(idx);
                    }
                    if let Some(call_id) = m.tool_call_id.as_ref()
                        && let Some(idx) = pending_tools.remove(call_id)
                    {
                        if let Some(Segment::Tool {
                            ok,
                            output,
                            preview,
                            preview_total,
                            ..
                        }) = self.segments.get_mut(idx)
                        {
                            *ok = Some(!m.is_error);
                            *output = m.content.clone();
                            (*preview, *preview_total) = view::tool_preview(None, &m.content);
                        }
                        self.touch_segment(idx);
                    }
                }
                Role::System => {}
            }
        }
        if let Some(user_index) = previous_user {
            self.restore_turn_notes(&turn_notes, user_index);
        }
        if let Some(seg_start) = work_start.take() {
            self.close_restored_group(
                seg_start,
                turn_user,
                &mut remaining_summaries,
                anchored_mode,
            );
        }
        // The panel is derived from the active structured plan; legacy session
        // to-do state is intentionally not loaded.
    }

    /// Close a restored work run `[seg_start..]` into an `ActivityGroup`,
    /// preferring the summary anchored to this turn's user message.
    /// Restored groups always fold shut, like live-finished ones.
    pub(super) fn close_restored_group(
        &mut self,
        seg_start: usize,
        turn_user: Option<usize>,
        remaining: &mut Vec<ActivitySummary>,
        anchored_mode: bool,
    ) {
        let answer = self.segments.len();
        let derived = self.build_activity_group((seg_start, answer));
        let saved = take_summary(remaining, anchored_mode, turn_user).unwrap_or(ActivitySummary {
            calls: derived.calls,
            thinking: derived.thinking,
            duration_ms: 0,
            errors: derived.errors,
            rejected: 0,
            user_index: turn_user,
        });
        self.activity_groups.push(ActivityGroup {
            seg_start,
            seg_end: answer,
            calls: saved.calls.max(derived.calls),
            thinking: saved.thinking,
            duration_ms: saved.duration_ms,
            errors: saved.errors,
            rejected: saved.rejected,
            turn_user: saved.user_index.or(turn_user),
        });
    }

    pub(super) fn restore_turn_notes(&mut self, notes: &[TurnNote], user_index: usize) {
        for note in notes.iter().filter(|note| note.user_index == user_index) {
            self.push_segment(Segment::Status {
                text: note.text.clone(),
                kind: if note.is_error {
                    StatusKind::Err
                } else {
                    StatusKind::Info
                },
                expanded: false,
                // restored notes are history, never transient: a retried turn
                // that later succeeded left no retry segment behind
                transient: false,
            });
        }
    }
}
