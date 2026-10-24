# -*- coding: utf-8 -*-
# Patch 3: loop_task/mod.rs — grants dirs, ctx seeding, read gate, journal, dispatch cleanup.

p = 'src/agent/loop_task/mod.rs'
s = open(p, encoding='utf-8').read()

# 3a. SessionGrants.dirs
old = """    /// mutating tools approved wholesale for the session (`edit`)
    pub(crate) tools: Vec<String>,
}"""
new = """    /// mutating tools approved wholesale for the session (`edit`)
    pub(crate) tools: Vec<String>,
    /// directories outside the root opened for the session ("always"),
    /// canonical prefixes
    pub(crate) dirs: Vec<PathBuf>,
}"""
assert s.count(old) == 1, "grants dirs"
s = s.replace(old, new)

# 3b. ctx seeding after construction — find the ctx build tail
old = """    let mut ctx = ToolCtx::with_read_only(&root, read_only)"""
assert s.count(old) == 1
# find the end of that statement chain: seed right after the line
idx = s.index(old)
end = s.index(";\n", idx) + 2
seed = """    // session-wide approval state rides the context: full access opens
    #![allow(clippy::needless_raw_string_hashes)]
"""
# simpler: append a seeding statement after the full builder chain ends — locate the next line starting with "    let " after idx
import re
m = re.search(r"\n(    let [a-z_]+)", s[end:])
insert_at = end + m.start() + 1
seeding = """ctx.outside_all = approval_mode == ApprovalMode::Full;
    ctx.outside_roots = approval_grants.dirs.clone();
    ctx.outside_granted = approval_grants.dirs.clone();
    """
s = s[:insert_at] + seeding + s[insert_at:]

# 3c. grants init lines
old = """    let mut always_allow: Vec<loop_ask::Grant> = approval_grants.bash;
    let mut tool_grants: Vec<String> = approval_grants.tools;"""
new = """    let mut always_allow: Vec<loop_ask::Grant> = approval_grants.bash;
    let mut tool_grants: Vec<String> = approval_grants.tools;"""
assert s.count(old) == 1

# 3d. both AgentOutcome constructions gain dirs (take from ctx at the end)
old = """                approval_grants: SessionGrants {
                    bash: always_allow,
                    tools: tool_grants,
                },"""
new = """                approval_grants: SessionGrants {
                    bash: always_allow,
                    tools: tool_grants,
                    dirs: ctx.outside_granted.clone(),
                },"""
assert s.count(old) == 1, "compact outcome"
s = s.replace(old, new)
old = """            approval_grants: SessionGrants {
                bash: always_allow,
                tools: tool_grants,
            },"""
new = """            approval_grants: SessionGrants {
                bash: always_allow,
                tools: tool_grants,
                dirs: std::mem::take(&mut ctx.outside_granted),
            },"""
assert s.count(old) == 1, "main outcome"
s = s.replace(old, new)

# 3e. user_msg journal record: model + approval attribution
old = """        if let Some(user_message) = messages.iter().rev().find(|m| m.role == Role::User) {
            let _ = writer.append("user_msg", serde_json::json!({
                "hash": format!("{:x}", sha2::Sha256::digest(user_message.content.as_bytes())),
                "chars": user_message.content.chars().count(),
                "goal_like": user_message.content.starts_with("goal:") || user_message.content.starts_with("/goal"),
            }));
        }"""
new = """        if let Some(user_message) = messages.iter().rev().find(|m| m.role == Role::User) {
            // per-turn attribution: the journal once credited this session
            // to a model it was not running on, because session_start names
            // only the first run's model. The turn record names the one
            // that actually answers, plus the approval axis it ran under.
            let _ = writer.append("user_msg", serde_json::json!({
                "hash": format!("{:x}", sha2::Sha256::digest(user_message.content.as_bytes())),
                "chars": user_message.content.chars().count(),
                "goal_like": user_message.content.starts_with("goal:") || user_message.content.starts_with("/goal"),
                "model": model_id,
                "approval": approval_mode.as_str(),
            }));
        }"""
assert s.count(old) == 1, "user_msg"
s = s.replace(old, new)

# 3f. session_start: approval axis
old = """            let _ = writer.session_start(
                &model_id,
                if plan_mode { "plan" } else { "act" },
                None,
                "unknown",
                resumed_from,
            );"""
new = """            let _ = writer.session_start(
                &model_id,
                if plan_mode { "plan" } else { "act" },
                approval_mode.as_str(),
                None,
                "unknown",
                resumed_from,
            );"""
assert s.count(old) == 1, "session_start call"
s = s.replace(old, new)

# 3g. read gate before the parallel batch
old = """                    let batch_end = parallel_read_only_batch_end(
                        &turn.calls,
                        call_index,
                        limits.max_parallel_tools.clamp(1, 64),
                    );
                    if batch_end - call_index > 1 {"""
new = """                    let batch_end = parallel_read_only_batch_end(
                        &turn.calls,
                        call_index,
                        limits.max_parallel_tools.clamp(1, 64),
                    );
                    // Outside reads earn the same boundary as writes: one
                    // approval opens the directory (run-only or session),
                    // full access never asks (`outside_all`). A denied read
                    // is precomputed into the batch outcomes so execution
                    // skips it cleanly.
                    if subagent_depth == 0 && approval_mode != ApprovalMode::Full {
                        for idx in call_index..batch_end.max(call_index + 1) {
                            let peer = &turn.calls[idx];
                            let Some(path) = loop_ask::read_call_path(&peer.name, &peer.args)
                            else {
                                continue;
                            };
                            if !ctx.outside_uncovered(&path) {
                                continue;
                            }
                            let grant_dir = ctx.outside_grant_dir(&path);
                            let preview = grant_dir
                                .as_ref()
                                .map(|d| format!("{}/**", d.display()))
                                .unwrap_or_default();
                            let decision = loop_ask::approval_dialog(
                                format!("{} {path}", peer.name),
                                loop_ask::OUTSIDE_READ_REASON,
                                preview,
                                &tx,
                                &mut ctl,
                                &mut next_id,
                            )
                            .await;
                            match decision {
                                Some(ApprovalDecision::Deny) => {
                                    parallel_outcomes.insert(
                                        idx,
                                        (
                                            0,
                                            tools::Outcome::err(format!(
                                                "{} denied by user ({})",
                                                peer.name,
                                                loop_ask::OUTSIDE_READ_REASON
                                            )),
                                            None,
                                        ),
                                    );
                                }
                                Some(ApprovalDecision::AlwaysSession) => {
                                    if let Some(d) = grant_dir {
                                        ctx.outside_roots.push(d.clone());
                                        ctx.outside_granted.push(d);
                                    }
                                }
                                Some(ApprovalDecision::RunOnce) => {
                                    if let Some(d) = grant_dir {
                                        ctx.outside_roots.push(d);
                                    }
                                }
                                None => {
                                    parallel_outcomes.insert(
                                        idx,
                                        (
                                            0,
                                            tools::Outcome::err("tui closed awaiting approval"),
                                            None,
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    if batch_end - call_index > 1 {"""
assert s.count(old) == 1, "read gate"
s = s.replace(old, new)

# 3h. bash_call call site: scratch outside_target
old = """                                subagent_depth,
                                approval_mode,
                                &mut journal,
                            )
                            .await
                        }"""
new = """                                subagent_depth,
                                approval_mode,
                                &mut journal,
                                &mut None,
                            )
                            .await
                        }"""
assert s.count(old) == 1, "bash call"
s = s.replace(old, new)

# 3i. dispatch: drop the per-call permit reset
old = """                            ctx.current_call_id = None;
                            // the outside-write permit covers exactly the
                            // confirmed call, never the rest of the batch
                            ctx.outside_write_permit = false;
                            out"""
new = """                            ctx.current_call_id = None;
                            out"""
assert s.count(old) == 1, "dispatch cleanup"
s = s.replace(old, new)

# 3j. import read_call_path/approval_dialog visibility via loop_ask::
s = s.replace(
    "use loop_ask::{ask_user, bash_call, mutating_tool_gate, run_tool_blocking};",
    "use loop_ask::{ask_user, bash_call, mutating_tool_gate, run_tool_blocking};",
)
open(p, 'w', encoding='utf-8', newline='').write(s)
print("mod.rs ok")
