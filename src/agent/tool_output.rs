//! What the request costs, and what the provider's own meter says about it.
//!
//! Tool results are the bulk of a long session — a measured run carried 776 KB
//! of them against 75 KB of conversation — so the token window is mostly a
//! budget for output the model has already seen. Nothing here rewrites that
//! output: a result is bounded **once, at the door** (`tools::bound_result`),
//! and from then on the transcript is append-only. The per-turn clearing pass
//! that used to live in this module rewrote old messages on every request, and
//! that is what cost the prefix cache (99% → 8% hits in a live 256k session)
//! and taught the model to ask back what it had just been given.
//!
//! What is left is the arithmetic the compaction gate needs: the provider's
//! bytes-per-token, measured rather than assumed, and the size of the request
//! once the reasoning ciphertext the wire stops replaying is taken out.

use crate::providers::{Message, Role};
use std::collections::HashMap;

/// Bytes per token until a provider says otherwise: the old constant, kept as
/// the cold-start value.
pub const DEFAULT_BYTES_PER_TOKEN: u64 = 4;
/// Lower clamp. Prose sits near 4 and code/JSON a little under it; below this a
/// request would have to be almost all short tokens.
pub const MIN_BYTES_PER_TOKEN: u64 = 3;
/// Upper clamp. Replayed ciphertext, HTML and escaped JSON measured 8.2 bytes
/// per token in a live session; a runaway value would let the pool grow past the
/// window the provider actually has.
pub const MAX_BYTES_PER_TOKEN: u64 = 12;
/// Smallest reported request whose ratio is trusted: a 10-token side call says
/// nothing about the shape of the agent's requests.
pub const RATIO_SAMPLE_TOKENS: u64 = 2_000;

/// The provider's own bytes-per-token for a request, clamped, or `None` when the
/// sample is too small to mean anything.
///
/// The context window is counted in tokens and the host only measures bytes, so
/// the conversion is a guess — and the guess was the whole size of two bugs: at
/// 4 bytes per token a 256k model got a 1 MB window while the provider billed its
/// 1,189,643-byte request as 144,424 tokens, so the real window was ~2 MB.
pub fn observe_bytes_per_token(total_bytes: u64, prompt_tokens: u64) -> Option<u64> {
    if prompt_tokens < RATIO_SAMPLE_TOKENS || total_bytes == 0 {
        return None;
    }
    // Rounded, not truncated: the ratio decides how many tokens the provider
    // will charge for the bytes the gate is counting, and flooring it biased the
    // whole estimate toward "full". A live session measured 456,931 bytes at
    // 120,930 tokens — 3.78 — and the truncated 3 priced the next request at
    // 205,914 tokens, firing a compaction that the real ratio put at 60% of the
    // window rather than the 80% the threshold says.
    Some(
        ((total_bytes as f64 / prompt_tokens as f64).round() as u64)
            .clamp(MIN_BYTES_PER_TOKEN, MAX_BYTES_PER_TOKEN),
    )
}

/// What the provider billed for one request, next to the bytes the host put on
/// it. The pair is the only honest meter of the next request: tokenizers price
/// base64 ciphertext and escaped JSON nothing like the prose the byte counter
/// assumes, so an estimate made from bytes alone drifts away from the window the
/// provider actually runs out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireAnchor {
    /// prompt tokens the provider reported for the anchored request
    pub tokens: u64,
    /// [`wire_message_bytes`] of that same request
    pub bytes: u64,
}

/// What the wire may carry, as one value: the model's window, the provider's
/// own bytes-per-token, and the last whole-transcript request it metered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wire {
    pub context_limit: u64,
    pub bytes_per_token: u64,
    /// The provider's own count for a request that carried this whole
    /// transcript, when one has been made. Only a whole-transcript request can
    /// anchor: a continuation sends a tail and bills a context the host never
    /// counted, so its `prompt_tokens` have no byte counterpart.
    pub measured: Option<WireAnchor>,
}

/// How many recent assistant turns keep their replayed reasoning on the wire.
pub const WIRE_REASONING_TURNS: usize = 1;

/// The provider-state keys that hold replayed chain-of-thought.
const REASONING_KEYS: [&str; 2] = ["reasoning_items", "thinking_blocks"];

/// Bytes per token to convert with: the provider's observed ratio, clamped, or
/// the cold-start constant when nothing has been observed (`Wire::default()`).
pub fn token_ratio(wire: &Wire) -> u64 {
    if wire.bytes_per_token == 0 {
        return DEFAULT_BYTES_PER_TOKEN;
    }
    wire.bytes_per_token
        .clamp(MIN_BYTES_PER_TOKEN, MAX_BYTES_PER_TOKEN)
}

/// What the messages of a request cost it, in bytes: turn text, call arguments,
/// the id a result answers, replayed provider state, and media priced through
/// the same ratio the caller converts with. Everything the token window is
/// spent on except the instruction prefix, which is the same on every request.
///
/// Text is priced with [`crate::providers::effective_text_bytes`]: real bytes
/// for ASCII, display cells at two bytes for wide glyphs, so a CJK transcript
/// is not billed at a quarter of what its tokenizer charges.
///
/// Media arrives as tokens over four ([`media_wire_bytes`]) and is re-priced at
/// `bytes_per_token`, so a screenshot is worth the same tokens whichever way the
/// window is measured.
pub fn wire_message_bytes(messages: &[Message], bytes_per_token: u64) -> u64 {
    let ratio = if bytes_per_token == 0 {
        DEFAULT_BYTES_PER_TOKEN
    } else {
        bytes_per_token
    };
    messages
        .iter()
        .map(|message| {
            crate::providers::effective_text_bytes(&message.content)
                + message.tool_call_id.as_deref().map_or(0, str::len) as u64
                + message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        call.name.len() as u64
                            + crate::providers::json_byte_len(&call.args)
                            + call
                                .extra_content
                                .as_ref()
                                .map(crate::providers::json_byte_len)
                                .unwrap_or(0)
                    })
                    .sum::<u64>()
                + message
                    .provider_state
                    .as_ref()
                    .map(crate::providers::json_byte_len)
                    .unwrap_or(0)
                + (media_wire_bytes(message) as u64 / 4).saturating_mul(ratio)
        })
        .sum()
}

/// What the transcript in `messages` would cost on the wire: shaped the way a
/// request is shaped — replay pruned, aged output stubbed — then counted. Used
/// both for the byte total of the request about to be sent and for the one that
/// was already billed, so the two are comparable.
pub fn wire_shaped_bytes(messages: &[Message], wire: &Wire) -> u64 {
    let mut shaped = messages.to_vec();
    prune_wire_reasoning(&mut shaped);
    wire_message_bytes(&shaped, token_ratio(wire))
}

pub fn prune_wire_reasoning(messages: &mut [Message]) {
    let mut carried = 0usize;
    for message in messages.iter_mut().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        let Some(obj) = message
            .provider_state
            .as_mut()
            .and_then(|state| state.as_object_mut())
        else {
            continue;
        };
        if !REASONING_KEYS.iter().any(|key| obj.contains_key(*key)) {
            continue;
        }
        if carried < WIRE_REASONING_TURNS {
            carried += 1;
            continue;
        }
        for key in REASONING_KEYS {
            obj.remove(key);
        }
        if obj.is_empty() {
            message.provider_state = None;
        }
    }
}

pub fn live_skill_names(messages: &[Message]) -> std::collections::BTreeSet<String> {
    let mut by_call: HashMap<&str, &str> = HashMap::new();
    for message in messages {
        for call in &message.tool_calls {
            if call.name != "skill" {
                continue;
            }
            if let Some(name) = call.args["name"].as_str() {
                by_call.insert(call.id.as_str(), name);
            }
        }
    }
    messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| {
            message
                .tool_call_id
                .as_deref()
                .and_then(|id| by_call.get(id).copied())
        })
        .map(|name| name.to_ascii_lowercase())
        .collect()
}

fn media_wire_bytes(message: &Message) -> usize {
    message
        .images
        .iter()
        .map(|image| crate::media::pressure_tokens(image.width, image.height) as usize * 4)
        .sum::<usize>()
        + message
            .documents
            .iter()
            .map(|document| crate::pdf::document_tokens(document.pages) as usize * 4)
            .sum::<usize>()
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Message, Role, ToolCallReq};
    use serde_json::json;

    fn exchange<S: AsRef<str>>(results: &[S]) -> Vec<Message> {
        let mut out = Vec::new();
        for (i, body) in results.iter().enumerate() {
            let body = body.as_ref();
            let id = format!("call_{i}");
            out.push(
                Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                    id.clone(),
                    "read",
                    json!({"file_path": format!("f{i}.rs")}),
                )]),
            );
            out.push(Message::tool_result(&id, body, false));
        }
        out
    }

    #[test]
    fn only_the_newest_turns_reasoning_rides_the_wire() {
        let blob = "A".repeat(30_000);
        let body = "w".repeat(100);
        let many: Vec<String> = (0..4).map(|_| body.clone()).collect();
        let mut messages = exchange(&many);
        for message in &mut messages {
            if message.role == Role::Assistant {
                message.provider_state = Some(json!({
                    "reasoning_items": [{"type": "reasoning", "encrypted_content": blob}],
                    "phase": "commentary"
                }));
            }
        }
        let mut wire = messages.clone();
        prune_wire_reasoning(&mut wire);

        let carried: Vec<&Message> = wire
            .iter()
            .filter(|m| {
                m.provider_state
                    .as_ref()
                    .is_some_and(|s| s.get("reasoning_items").is_some())
            })
            .collect();
        assert_eq!(
            carried.len(),
            WIRE_REASONING_TURNS,
            "one turn's ciphertext survives, the rest is dropped"
        );
        let survivor = carried[0].provider_state.as_ref().expect("carried turn");
        assert_eq!(
            survivor.get("phase").and_then(|v| v.as_str()),
            Some("commentary"),
            "only the reasoning keys are stripped, other provider state rides on"
        );
        let priced = |m: &[Message]| wire_message_bytes(m, DEFAULT_BYTES_PER_TOKEN);
        assert!(
            priced(&wire) * 3 < priced(&messages),
            "three of the four turns' ciphertext still rides the wire: {} against {}",
            priced(&wire),
            priced(&messages)
        );
        assert!(
            priced(&wire) > priced(&messages) / 6,
            "the carried turn's ciphertext was not counted at all"
        );
    }

    #[test]
    fn the_observed_ratio_rounds_instead_of_truncating() {
        // 456,931 bytes billed at 120,930 tokens is 3.78 bytes per token. The
        // truncated 3 priced the following request at 205,914 tokens and fired a
        // compaction the real ratio put at 60% of the window, not 80%.
        assert_eq!(observe_bytes_per_token(456_931, 120_930), Some(4));
        assert_eq!(observe_bytes_per_token(15_000, 4_000), Some(4));
        assert_eq!(observe_bytes_per_token(12_000, 4_000), Some(3));
        // the floor still catches a request denser than any prose
        assert_eq!(observe_bytes_per_token(8_000, 4_000), Some(3));
        // and a sample too small to say anything about the shape is not used
        assert_eq!(observe_bytes_per_token(40_000, 10), None);
    }
}
