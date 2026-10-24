//! Tool execution context: project root, session guards, read tracking.
//!
//! Moved byte-for-byte from `tools/mod.rs`; behavior unchanged.
//!
use crate::plan;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct ToolCtx {
    /// project root; every path must resolve inside it
    pub root: PathBuf,
    /// canonicalized [`Self::root`], computed once at construction so the
    /// jail check in [`Self::resolve`] does not canonicalize the root on
    /// every call. Falls back to [`Self::root`] when canonicalization fails;
    /// every tool op fails downstream in that case anyway.
    root_canon: PathBuf,
    /// secondary project instances can inspect but not mutate project state
    pub read_only: bool,
    /// Full access: every path resolves, inside or out (session-level;
    /// set once per run from the approval mode).
    pub outside_all: bool,
    /// Approved leaps outside the project root, as canonical directory
    /// prefixes. The jail is a prompt, not a wall: a confirmed directory
    /// opens reads AND writes under it. Seeded from the session's grants,
    /// extended live by approvals ("run once" for this run, "always" also
    /// into `outside_granted` for persistence).
    pub outside_roots: Vec<PathBuf>,
    /// The subset of `outside_roots` earned with an "always this session"
    /// answer — harvested back into the session grants at turn end.
    pub outside_granted: Vec<PathBuf>,
    /// Session this context belongs to. keys the shadow snapshot chain
    /// on it (`refs/sessions/<id>`), so two sessions in one project do not
    /// interleave their checkpoint history — and retention can truncate one
    /// session's chain without touching another's.
    pub session_id: String,
    /// Override for the checkpoint chain only: a child session's
    /// snapshots land on its parent's chain, so the parent's `/undo` sees
    /// the child's step boundaries and bash mutations. `None` means the own
    /// chain. Job isolation, read-guard and journal identity all stay on
    /// `session_id` — only the shadow chain is shared.
    pub checkpoint_session: Option<String>,
    /// Files read this session and the content hash they had at the time,
    /// keyed by canonical path. calls for the guard to be hash-tracked: a
    /// file changed by `bash` since the last read has to be read again, and
    /// with paths alone the model could edit it blind. The canonical key also
    /// stops `read("src/x.rs")` followed by `edit("./src/x.rs")` from being
    /// refused as unread.
    pub files_read: HashMap<PathBuf, String>,
    /// Served read windows this session, per canonical path. A request whose
    /// lines are already covered is answered with a pointer back at the earlier
    /// output instead of the bytes again: a live session spent 23 of its 45
    /// reads on one 5300-line file, five of them inside lines 846–926. Keyed by
    /// content hash, so an edit or a `bash` write puts the file back on the
    /// fresh path.
    pub read_windows: HashMap<PathBuf, Served>,
    /// journal of checkpoints created by this session's mutations
    pub journal: Vec<(String, String)>,
    /// Host limits on the plan, and the model context they are derived from.
    /// The budget used to come from a `context_limit` the model passed in its
    /// own tool arguments.
    pub plan_limits: crate::config::PlanConfig,
    /// context window of the model driving this session, in tokens
    pub context_limit: u64,
    /// set from the TUI when the user presses Esc during a
    /// running tool. Long-running handlers poll it at their existing
    /// wait/retry points and stop cooperatively — nothing here can reach into
    /// a handler and pull it out mid-syscall, so a handler that never checks
    /// this is a handler Esc cannot interrupt.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Where the shadow repository lives, from [undo].shadow configuration.
    pub shadow_store: crate::config::ShadowStore,
    /// Step this session currently holds. Set on `plan start`,
    /// cleared on `finish`/`block`/`cancel`; the loop mirrors it into the
    /// journal attribution at dispatch. `None` means idle.
    pub current_step: Option<String>,
    /// Immutable spawn context for subagent sessions. `None` for
    /// the main agent. Mutating tools refuse to run when the inherited
    /// epoch no longer matches the plan.
    pub subagent_step: Option<plan::StepContext>,
    /// Write scope for a writer subagent (`None` = unrestricted): every
    /// file mutation must sit inside these project-relative roots. Taken
    /// from the spawn registry at construction; read-only children and
    /// the main agent never set it.
    pub subagent_write_paths: Option<Vec<String>>,
    /// Session id of the spawning agent, if any (audit H1). A child starts
    /// with an empty journal, so its own taint reads clean no matter what
    /// the parent consumed; trust gates must see the parent's external
    /// exposure too. `None` for main agents.
    pub parent_session: Option<String>,
    /// Frozen inputs the loop approved for this call after a user approval
    /// dialog (additive writes never land here — they flow without asking).
    /// Checked by dispatch; the loop owns the lifetime (one-shot vs session
    /// grants), so blocking dispatch clones never need merging back.
    /// Slow baseline capture (baselines, snapshots, differentials) is
    /// deferred to a background worker instead of blocking the tool call.
    /// Set by the interactive loop; tests and headless runs keep the
    /// synchronous capture so results stay deterministic.
    pub background_baselines: bool,
    /// `call_id` of the tool call currently executing, used to attribute read
    /// windows so windows whose `tool_result` was later cleared from the wire
    /// can be evicted before answering "already cover X–Y, see above".
    pub current_call_id: Option<String>,
    /// Skill discovery configuration (`[skills]` in `config.toml`).
    pub skills_cfg: crate::config::SkillsConfig,
    /// Lowercased names of skills already loaded via `skill(name="...")` in
    /// this session so repeat calls can return a concise pointer instead of
    /// duplicating the full `SKILL.md` body.
    pub activated_skills: std::collections::BTreeSet<String>,
    /// Whether the driving model accepts image input. `true` when the catalog
    /// says so or says nothing; only a declared `vision = false` blocks
    /// `view_image`.
    pub vision: bool,
    /// Whether the model accepts PDF documents. Absent/unknown is `false`:
    /// a PDF sent to a model without document support is a hard API error.
    pub pdf: bool,
    /// `[tools]` caps for the built-in tools.
    pub tools: crate::config::ToolsConfig,
    /// `[limits]` concurrency caps shared with the loop.
    pub limits: crate::config::LimitsConfig,
    /// Media ids that are still visible on the wire. `view_image` answers a
    /// repeat view with a pointer while the earlier pixels are still in
    /// context; once the result is cleared, a re-view attaches them again.
    pub visible_images: std::sync::Arc<std::collections::HashSet<String>>,
    /// Document ids still visible on the wire; same contract as
    /// [`Self::visible_images`] for `view_document`.
    pub visible_documents: std::sync::Arc<std::collections::HashSet<String>>,
}

impl ToolCtx {
    #[cfg(test)]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_read_only(root, false)
    }

    pub fn with_read_only(root: impl Into<PathBuf>, read_only: bool) -> Self {
        let root = root.into();
        // Canonicalize once: `resolve` compares canonical paths, and the root
        // itself may sit behind a symlink (macOS `/var` -> `/private/var`).
        let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
        Self {
            root,
            root_canon,
            read_only,
            outside_all: false,
            outside_roots: Vec::new(),
            outside_granted: Vec::new(),
            session_id: "shared".into(),
            checkpoint_session: None,
            files_read: HashMap::new(),
            read_windows: HashMap::new(),
            journal: Vec::new(),
            plan_limits: crate::config::PlanConfig::default(),
            context_limit: 0,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shadow_store: crate::config::ShadowStore::Local,
            current_step: None,
            subagent_step: None,
            subagent_write_paths: None,
            parent_session: None,
            background_baselines: false,
            current_call_id: None,
            skills_cfg: crate::config::SkillsConfig::default(),
            activated_skills: std::collections::BTreeSet::new(),
            vision: true,
            pdf: false,
            tools: Default::default(),
            limits: Default::default(),
            visible_images: std::sync::Arc::new(std::collections::HashSet::new()),
            visible_documents: std::sync::Arc::new(std::collections::HashSet::new()),
        }
    }

    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    pub fn with_pdf(mut self, pdf: bool) -> Self {
        self.pdf = pdf;
        self
    }

    /// Refresh the ids still visible on the wire; called before a tool batch.
    pub fn set_visible_images(&mut self, ids: std::collections::HashSet<String>) {
        self.visible_images = std::sync::Arc::new(ids);
    }

    /// A result with pixels just joined the live context. Kept even when the
    /// set is shared with clones: losing the update would only risk a
    /// duplicate image later, never a missing one.
    pub fn note_visible_image(&mut self, id: &str) {
        let mut ids = (*self.visible_images).clone();
        ids.insert(id.to_string());
        self.visible_images = std::sync::Arc::new(ids);
    }

    /// Refresh the document ids still visible on the wire; called before a
    /// tool batch, mirroring [`Self::set_visible_images`].
    pub fn set_visible_documents(&mut self, ids: std::collections::HashSet<String>) {
        self.visible_documents = std::sync::Arc::new(ids);
    }

    pub fn note_visible_document(&mut self, id: &str) {
        let mut ids = (*self.visible_documents).clone();
        ids.insert(id.to_string());
        self.visible_documents = std::sync::Arc::new(ids);
    }

    /// External taint that trust gates must honor: this session's own
    /// journal, OR the spawning parent's when this is a child (audit H1 —
    /// a fresh child journal reads clean no matter what the parent
    /// consumed, and the task text itself may be shaped by it).
    pub fn external_taint(&self) -> bool {
        crate::agent::trust::taint_level(&self.root, &self.session_id).external
            || self
                .parent_session
                .as_deref()
                .is_some_and(|p| crate::agent::trust::taint_level(&self.root, p).external)
    }

    /// Canonical path of the host-owned state dir (`.sqwai/`). Walks must
    /// skip it explicitly: `hidden(true)` checks attributes on Windows, not
    /// dotfiles, and the comparison has to run on canonical paths because
    /// the root itself may sit behind a symlink (macOS `/var`).
    pub fn host_state_dir(&self) -> PathBuf {
        self.root_canon.join(STATE_DIR)
    }

    /// Where oversized tool output goes instead of dying in a truncation
    /// marker: disposable, retention-pruned, and inside the host state the
    /// project already ignores. It used to sit at the project root, where it
    /// showed up in `ls` as if it were part of the codebase.
    pub fn spill_dir(&self) -> PathBuf {
        self.host_state_dir().join("spill")
    }

    pub fn with_shadow_store(mut self, shadow_store: crate::config::ShadowStore) -> Self {
        self.shadow_store = shadow_store;
        self
    }

    /// Name the session whose checkpoint chain this context appends to.
    pub fn in_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// Whose shadow chain snapshots go to (child → parent). Everything else
    /// keeps using `session_id`.
    pub fn checkpoint_chain(&self) -> &str {
        self.checkpoint_session
            .as_deref()
            .unwrap_or(&self.session_id)
    }

    /// Share a cancellation flag across this context and its clones. The
    /// caller keeps the `Arc` and flips it; every clone taken afterwards
    /// (including the one moved into a `spawn_blocking` closure) observes it.
    pub fn with_cancel(mut self, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Adopt the host's plan limits and the driving model's context window.
    pub fn with_plan_limits(
        mut self,
        plan_limits: crate::config::PlanConfig,
        context_limit: u64,
    ) -> Self {
        self.plan_limits = plan_limits;
        self.context_limit = context_limit;
        self
    }

    /// resolve a user-supplied path inside the project; rejects escapes and
    /// host-owned state under `.sqwai/`. Reading is the permissive
    /// direction: spilled tool output under `.sqwai/spill/` stays reachable,
    /// because a pointer the host handed out has to be one the model can
    /// follow. Mutations go through
    /// [`resolve_writable`](Self::resolve_writable), which does not.
    pub fn resolve(&self, p: &str) -> Result<PathBuf, String> {
        self.resolve_parts(p, false).map(|(joined, _)| joined)
    }

    /// [`resolve`](Self::resolve) for an op that is about to change the file.
    pub fn resolve_writable(&self, p: &str) -> Result<PathBuf, String> {
        self.resolve_parts(p, true).map(|(joined, _)| joined)
    }

    /// Canonical project-relative spelling (forward-slashed) of what `p`
    /// actually points at. Scope gates must decide on this, not on the
    /// lexical spelling: the OS resolves symlinks, so `src/link/x` with
    /// `src/link -> b/` writes into `b/` (audit H3). Same jail as
    /// [`resolve`](Self::resolve), and treated as a write target — which is
    /// what scope gates are deciding about.
    pub fn resolve_scope_rel(&self, p: &str) -> Result<String, String> {
        self.resolve_parts(p, true)
            .map(|(_, rel)| rel.to_string_lossy().replace('\\', "/"))
    }

    /// Join `p` onto the root (absolutes pass through) and canonicalize the
    /// deepest existing ancestor, so `..` and symlinks cannot smuggle a
    /// path past the prefix check while files that do not exist yet still
    /// resolve. Shared by the jail, the scope gates and the escape probes.
    fn resolve_target(&self, p: &str) -> Result<(PathBuf, PathBuf), String> {
        let joined = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else if let Some(win) = unix_drive_path(p) {
            // `/c/Users/...` is how shells on Windows spell `C:\Users\...`.
            // Joining it onto the root instead once built a phantom
            // `C:\c\Users\...` tree that a live session wrote 13 files
            // into while the real target stayed empty — so this normalizes
            // instead of guessing.
            win
        } else if p.starts_with('/') || p.starts_with('\\') {
            // rooted but no drive: joining would silently replace the root's
            // tail (`/tmp/x` -> `C:\tmp\x`). Refuse and say the spelling.
            return Err(format!(
                "path '{p}' is rooted but has no drive — spell it as an                  absolute Windows path (C:\\...) or relative to the project"
            ));
        } else {
            self.root.join(p)
        };
        let mut anc = joined.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !anc.exists() {
            let Some(name) = anc.file_name().map(|n| n.to_os_string()) else {
                break;
            };
            match anc.parent() {
                Some(par) => {
                    tail.push(name);
                    anc = par.to_path_buf();
                }
                None => break,
            }
        }
        let canon = anc
            .canonicalize()
            .map_err(|e| format!("cannot resolve path {}: {e}", joined.display()))?;
        let mut resolved = canon;
        for name in tail.iter().rev() {
            resolved.push(name);
        }
        Ok((joined, resolved))
    }

    /// Does `p` land outside the project root? The question behind the
    /// write jail: an outside write is an approval, not a refusal. An
    /// unresolvable path is not an escape — the jail itself errors on it
    /// if the call ever runs.
    pub fn write_escapes(&self, p: &str) -> bool {
        match self.resolve_target(p) {
            Ok((_, resolved)) => resolved.strip_prefix(&self.root_canon).is_err(),
            Err(_) => false,
        }
    }

    /// The same question for a raw shell operand or redirect destination:
    /// relative targets resolve against the working directory (the root),
    /// surrounding quotes come off. Null sinks are the caller's filter.
    pub fn shell_target_escapes(&self, target: &str) -> bool {
        let t = target.trim();
        let t = t.trim_matches(|c| c == '"' || c == '\'');
        !t.is_empty() && self.write_escapes(t)
    }

    /// Is `p` outside the root AND outside every approved directory?
    pub fn outside_uncovered(&self, p: &str) -> bool {
        match self.resolve_target(p.trim()) {
            Ok((_, resolved)) => {
                resolved.strip_prefix(&self.root_canon).is_err()
                    && !self.outside_roots.iter().any(|r| resolved.starts_with(r))
            }
            Err(_) => false,
        }
    }

    /// The directory an approval of `p` should open: the path itself when
    /// it is an existing directory, its parent for a file target. `None`
    /// when the path does not resolve.
    pub fn outside_grant_dir(&self, p: &str) -> Option<PathBuf> {
        let (_, resolved) = self.resolve_target(p.trim()).ok()?;
        if resolved.is_dir() {
            Some(resolved)
        } else {
            resolved.parent().map(|par| par.to_path_buf())
        }
    }

    /// Shared walk behind [`resolve`](Self::resolve) and
    /// [`resolve_scope_rel`](Self::resolve_scope_rel): returns the
    /// display spelling (uncanonicalized join) and the canonical
    /// project-relative path the security decisions run on.
    fn resolve_parts(&self, p: &str, mutating: bool) -> Result<(PathBuf, PathBuf), String> {
        let p_trimmed = p.trim().trim_start_matches("./").trim_start_matches(".\\");
        let redirected;
        let p = if !mutating
            && p_trimmed.eq_ignore_ascii_case("MEMORY.md")
            && !self.root.join(p_trimmed).exists()
        {
            redirected = ".sqwai/memory/MEMORY.md".to_string();
            redirected.as_str()
        } else {
            p
        };
        let (joined, resolved) = self.resolve_target(p)?;
        let Ok(relative) = resolved.strip_prefix(&self.root_canon) else {
            // the jail is a prompt, not a wall: full access resolves every
            // path, and a confirmed directory opens reads AND writes under
            // it (the read-guard refills for outside files too). Host-owned
            // state is root-relative — it cannot reach this branch.
            if self.outside_all || self.outside_roots.iter().any(|r| resolved.starts_with(r)) {
                return Ok((joined, resolved));
            }
            return Err(format!(
                "path '{}' escapes the project directory",
                joined.display()
            ));
        };
        // Plan, journal, memory and graph are reachable only through their own
        // tools. Enforcing it here is what makes "host-written" and
        // "append-only" guarantees rather than requests.
        if let Some(denied) = host_owned_denial(relative, mutating) {
            return Err(denied);
        }
        // The display spelling is `joined`, not `resolved`: the whole display
        // layer (`rel_label`, checkpoint labels, `FileDiff.path`) strips the
        // *uncanonicalized* root, and where the root itself is reached
        // through a symlink (macOS `/var` -> `/private/var`) a canonical
        // path would miss that prefix and leak absolute machine-specific
        // paths into the journal. The security decision above was already
        // made in canonical space, so the spelling returned here only
        // affects labels, never the verdict.
        Ok((joined, relative.to_path_buf()))
    }

    fn read_key(p: &Path) -> PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    pub(crate) fn mark_read(&mut self, p: &Path) {
        let hash = file_hash(p);
        self.files_read.insert(Self::read_key(p), hash);
    }

    /// Whether the file may be edited: it was read, and it still holds what it
    /// held then.
    pub(crate) fn read_state(&self, p: &Path) -> ReadState {
        match self.files_read.get(&Self::read_key(p)) {
            None => ReadState::Unread,
            Some(seen) if *seen == file_hash(p) => ReadState::Current,
            Some(_) => ReadState::Stale,
        }
    }

    /// True when every line of `from..=to` was already served from identical
    /// content — including the case where an earlier, wider window covers it.
    /// If the previous read of this file already returned the stub once
    /// (`stubbed == true`) and the model is asking again, returns `None` so
    /// the model is not trapped in a refusal loop when it needs exact lines for `edit`.
    pub(crate) fn read_range_served(
        &mut self,
        p: &Path,
        hash: &str,
        from: usize,
        to: usize,
    ) -> Option<(usize, usize)> {
        let key = Self::read_key(p);
        let served = self.read_windows.get_mut(&key)?;
        if served.hash != hash {
            return None;
        }
        let hit = served
            .ranges
            .iter()
            .find(|(a, b)| *a <= from && *b >= to)
            .copied();
        if hit.is_some() {
            if served.stubbed {
                served.stubbed = false;
                return None;
            }
            served.stubbed = true;
        }
        hit
    }

    /// Record the range that was actually put in front of the model, merging
    /// touching ranges so the map cannot grow with every window.
    pub(crate) fn note_read_window(&mut self, p: &Path, from: usize, to: usize, hash: String) {
        let key = Self::read_key(p);
        let call_id = self.current_call_id.clone();
        let served = self.read_windows.entry(key).or_insert_with(Served::new);
        if served.hash != hash {
            served.hash = hash;
            served.ranges.clear();
            served.windows.clear();
            served.served = 0;
            served.stubbed = false;
        }
        served.served += 1;
        served.stubbed = false;
        served.windows.push((from, to, call_id));
        served.rebuild_ranges();
    }

    /// Evict read windows whose `tool_result` is no longer visible on the wire
    /// (either cleared by `tool_output::trim_for_wire` or dropped by compaction),
    /// so `fs::read` never claims "see above" for output the model cannot see.
    pub(crate) fn retain_visible_read_windows(
        &mut self,
        visible_call_ids: &std::collections::HashSet<String>,
    ) {
        for served in self.read_windows.values_mut() {
            let before = served.windows.len();
            served
                .windows
                .retain(|(_, _, cid)| cid.as_ref().is_none_or(|id| visible_call_ids.contains(id)));
            if served.windows.len() != before {
                served.served = served.windows.len();
                served.rebuild_ranges();
            }
        }
    }

    /// How many windows of this file the model has been served from identical
    /// content — the counter behind the slicing nudge.
    pub(crate) fn read_window_count(&self, p: &Path, hash: &str) -> usize {
        self.read_windows
            .get(&Self::read_key(p))
            .filter(|served| served.hash == hash)
            .map(|served| served.served)
            .unwrap_or(0)
    }

    /// Merge `files_read` and `read_windows` accumulated by a parallel
    /// read-only worker back into the session context in deterministic order.
    pub(crate) fn merge_read_state(&mut self, child: ToolCtx) {
        for (path, hash) in child.files_read {
            self.files_read.insert(path, hash);
        }
        for (path, child_served) in child.read_windows {
            let slot = self.read_windows.entry(path).or_insert_with(Served::new);
            if slot.hash != child_served.hash {
                *slot = child_served;
            } else {
                for w in child_served.windows {
                    if !slot.windows.contains(&w) {
                        slot.windows.push(w);
                    }
                }
                slot.served = slot.windows.len();
                slot.rebuild_ranges();
            }
        }
        self.activated_skills.extend(child.activated_skills);
    }
}

/// What `read` has already served from one content version of a file: the hash
/// it served, the merged line ranges, and how many separate windows.
#[derive(Debug, Clone)]
pub struct Served {
    pub hash: String,
    pub ranges: Vec<(usize, usize)>,
    pub windows: Vec<(usize, usize, Option<String>)>,
    pub served: usize,
    pub stubbed: bool,
}

impl Served {
    fn new() -> Self {
        Self {
            hash: String::new(),
            ranges: Vec::new(),
            windows: Vec::new(),
            served: 0,
            stubbed: false,
        }
    }

    fn rebuild_ranges(&mut self) {
        let mut raw: Vec<(usize, usize)> = self.windows.iter().map(|(a, b, _)| (*a, *b)).collect();
        raw.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(raw.len());
        for (a, b) in raw {
            match merged.last_mut() {
                // touching ranges count as contiguous: 1–80 then 81–120 is 1–120
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        self.ranges = merged;
    }
}
/// What the read guard knows about a file the model wants to edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadState {
    /// never read in this session
    Unread,
    /// read, and unchanged since
    Current,
    /// read, but something changed it afterwards — `bash`, a formatter, the
    /// user's editor
    Stale,
}

/// Content hash of a file, matching what `file_diff` records. A missing or
/// unreadable file hashes to the empty string, which never equals a recorded
/// hash, so it reads as stale rather than as current.
fn file_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            format!("{:x}", hasher.finalize())
        }
        Err(_) => String::new(),
    }
}

/// The agent's own state directory. File tools see only `skills/`,
/// `config.toml` and spilled tool output under `spill/` inside it; plan,
/// journal, memory and checkpoints are host-owned and reachable only through
/// their dedicated tools.
const STATE_DIR: &str = ".sqwai";

/// Deny message when `relative` points at host-owned state, `None` otherwise.
/// `relative` must already be relative to the canonicalized project root, so a
/// symlink pointing into `.sqwai/` cannot slip past this check.
/// `mutating` is whether the caller is about to change the file: spilled tool
/// output may be read back but never written, because a file the model plants
/// in the spill dir would come to look like its own past evidence.
fn host_owned_denial(relative: &Path, mutating: bool) -> Option<String> {
    use std::path::Component;
    let mut components = relative.components();
    match components.next() {
        Some(Component::Normal(first)) if first == STATE_DIR => {}
        _ => return None,
    }
    let allowed = match components.next() {
        Some(Component::Normal(second)) if second == "skills" || second == "config.toml" => true,
        // `.sqwai/spill/<file>` and `.sqwai/memory/MEMORY.md`: read-only access
        // is allowed so the model can inspect spilled output or project memory,
        // while mutations stay blocked so writes must go through `memory_write`.
        Some(Component::Normal(second))
            if (second == "spill" || second == "memory") && !mutating =>
        {
            matches!(components.next(), Some(Component::Normal(_)))
        }
        // `.sqwai` itself: listing or writing the directory is not allowed
        _ => false,
    };
    if allowed {
        return None;
    }
    Some(format!(
        "path '{}' is host-owned state: the plan, the journal, memory and checkpoints are \
         reachable only through their own tools (plan, note, journal, \
         memory_write). Inside {STATE_DIR}/ the file tools may use skills/, config.toml and \
         files under spill/ (read only) — nothing else.",
        relative.display()
    ))
}

/// `/c/...` (and `/cygdrive/c/...`) spelled as a Windows drive path.
/// Only reached where `is_absolute()` is false — on Unix, `/c/...` is
/// already absolute and never lands here.
fn unix_drive_path(p: &str) -> Option<PathBuf> {
    let rest = p.strip_prefix('/')?;
    let mut parts = rest.splitn(2, '/');
    let first = parts.next()?;
    let tail = parts.next().unwrap_or("");
    let drive = if first.eq_ignore_ascii_case("cygdrive") {
        tail.split('/').next()?.chars().next()?
    } else {
        first.chars().next()?
    };
    let tail = if first.eq_ignore_ascii_case("cygdrive") {
        tail.split_once('/').map(|(_, t)| t).unwrap_or("")
    } else {
        tail
    };
    // a drive segment is exactly one letter (`/c/...`); anything longer
    // (`/tmp/...`, `/usr/...`) is a Unix absolute path, not a drive
    if first.chars().count() != 1 || !drive.is_ascii_alphabetic() {
        return None;
    }
    Some(PathBuf::from(format!(
        "{}:/{}",
        drive.to_ascii_uppercase(),
        tail
    )))
}

#[cfg(test)]
mod outside_jail_tests {
    use super::*;

    #[test]
    fn write_escapes_spots_outside_and_inside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ToolCtx::new(&root);
        // inside: plain relative, and a `..` walk that comes back in
        assert!(!ctx.write_escapes("src/main.rs"));
        let back_inside = format!("{}/proj/src/main.rs", dir.path().display());
        assert!(!ctx.write_escapes(&back_inside));
        // outside: sibling of the root, absolute or spelled relatively
        assert!(ctx.write_escapes("../outside.txt"));
        assert!(ctx.write_escapes(dir.path().join("outside.txt").to_string_lossy().as_ref()));
    }

    #[test]
    fn git_bash_drive_paths_resolve_to_the_real_drive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = ToolCtx::new(&root);
        // `/c/...` normalizes instead of building the phantom `C:\c\...`
        let (joined, resolved) = ctx.resolve_target("/c/Users/x/a.txt").unwrap();
        assert!(joined.to_string_lossy().starts_with("C:/Users/x/"), "{joined:?}");
        assert!(resolved.to_string_lossy().contains("Users"), "{resolved:?}");
        // ...including when the target is the project itself
        let root_unix = format!(
            "/{}",
            root.to_string_lossy().replace('\\', "/").replace(':', "/")
        );
        let proj_unix = format!("{root_unix}/src/main.rs");
        assert!(!ctx.write_escapes(&proj_unix), "{proj_unix}");
    }

    #[test]
    fn rooted_paths_without_a_drive_refuse_instead_of_phantom_joining() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(dir.path().join("proj"));
        let err = ctx.resolve_writable("/tmp/notes.txt").unwrap_err();
        assert!(err.contains("no drive"), "{err}");
    }

    #[test]
    fn an_approved_directory_opens_reads_and_writes_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("other");
        std::fs::create_dir_all(&outside).unwrap();
        let mut ctx = ToolCtx::new(&root);
        // nothing approved: both directions refuse
        assert!(ctx.resolve_writable("../other/a.txt").is_err());
        assert!(ctx.resolve("../other/a.txt").is_err());
        // full access resolves everything
        ctx.outside_all = true;
        assert!(ctx.resolve_writable("../other/a.txt").is_ok());
        assert!(ctx.resolve("../other/a.txt").is_ok());
        // an approved directory opens exactly its subtree; the grant is
        // stored canonicalized, the same shape resolve_target produces
        ctx.outside_all = false;
        ctx.outside_roots.push(outside.canonicalize().unwrap());
        assert!(ctx.resolve_writable("../other/a.txt").is_ok());
        assert!(ctx.resolve("../other/a.txt").is_ok());
        assert!(ctx.resolve("../third/b.txt").is_err());
    }
}
