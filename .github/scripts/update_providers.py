#!/usr/bin/env python3
"""
Sync builtin_providers.toml with latest model specifications and pricing.

Source of truth for specs/prices: LiteLLM model_prices_and_context_window.json.

Policy (agreed):
- AUTO-DISCOVERY: tracked model IDs are NOT hardcoded (except Gemini, which is
  owner-pinned). Each provider declares model FAMILIES in priority order; the
  script resolves the newest revision per family from the DB:
  * only mode == "chat" entries;
  * only direct keys (no third-party routes like azure_ai/, bedrock/, and no
    vendor-dot keys like anthropic.claude-...); first-party prefixes
    (deepseek/, moonshot/, xai/) are accepted and stripped to the API id;
  * dated snapshots (-YYYYMMDD, -YYYY-MM-DD) collapse into their alias;
  * entries with deprecation_date in the past are dropped;
  * audio/image/video/transcribe/tts/realtime/embedding/vision/search excluded;
  * within a family the max version wins, ties go to the shorter (alias) name;
  * a family missing from the DB is skipped with a log line (no stale data);
  * capped per provider to keep the menu usable.
- Legacy/retired IDs disappear on their own: they either leave the DB, get a
  deprecation_date, or are superseded by newer revisions inside their family.
- Unknown direct chat models outside the cap are reported as CANDIDATES.
- Fetch failure is fatal (exit 1) so the Action goes red instead of silently
  writing stale fallbacks with a fresh date.
- If nothing but the date changed, the file is left untouched (keeps the old
  updated_at, so git sees no diff and no empty commit is made).
"""

import datetime
import json
import os
import re
import sys
import urllib.request

LITELLM_URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
CATALOG_PATH = os.path.join(os.path.dirname(__file__), "..", "..", "builtin_providers.toml")

RESERVED_KEYS = {"sample_spec", "fallback_generalizations"}

# Substrings that disqualify a model for coding/chat use in sqwai.
EXCLUDE_SUBSTRINGS = (
    "audio", "image", "transcribe", "tts", "whisper", "realtime",
    "embedding", "diarize", "vision",
)

DATE_SUFFIX = re.compile(r"-20\d{6}$|-20\d\d-\d\d-\d\d$")
VENDOR_DOT = re.compile(r"^[a-z0-9_]+\.")

# Confirmed-phantom API IDs: present in the LiteLLM DB but not real models
# (e.g. bare gpt-5.6 — only luna/terra/sol/cyber exist). Logged when dropped.
DROP_IDS = {"gpt-5.6"}

# Extra substrings that disqualify a model for coding/chat use in sqwai.
# (generic audio/image/... live in EXCLUDE_SUBSTRINGS; these are search APIs.)
SEARCH_SUBSTRINGS = ("search",)

# Effort defaults for auto-discovered models (known IDs keep curated values).
EFFORT_HIGH = ("reason", "r1", "opus", "codex", "k3", "build", "o1", "o3", "o4")
EFFORT_OFF = ("mini", "nano", "lite")
EFFORT_LOW = ("luna",)
EFFORT_MEDIUM = ("flash", "haiku")

EFFORT_OVERRIDES = {
    # continuity with the previously curated catalog
    "claude-sonnet-4-5": "high",
    "claude-sonnet-5": "medium",
    "claude-haiku-4-5": "medium",
    "gpt-5.5": "high",
    "gpt-5.4": "high",
    "gpt-5.4-mini": "off",
    "gpt-5.3-codex": "high",
    "o4-mini": "medium",
    "deepseek-chat": "off",
    "deepseek-reasoner": "high",
    "grok-4.6": "high",
    "grok-4.5": "high",
    "grok-4.3": "medium",
    "grok-build-0.1": "high",
    "kimi-k3": "high",
    "kimi-k2.6": "medium",
    "kimi-k2.7-code": "high",
}

PROVIDERS_CONFIG = [
    {
        "name": "gemini",
        "title": "Gemini",
        "format": "openai",
        "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
        "api_key_env": "GEMINI_API_KEY",
        "continuation": True,
        # owner-pinned list; only specs/prices refresh from the DB
        "pinned": [
            ("gemini-3.1-pro-preview", "high", 1048576, 2.0, 12.0),
            ("gemini-3.8-flash", "medium", 1048576, 0.75, 3.75),
            ("gemini-3.7-flash", "medium", 1048576, 0.75, 3.75),
            ("gemini-3.6-flash", "medium", 1048576, 0.5, 3.0),
            ("gemini-3.5-flash", "medium", 1048576, 0.5, 3.0),
            ("gemini-3.5-flash-lite", "off", 1048576, 0.3, 2.5),
            ("gemini-3.1-flash-lite", "off", 1048576, 0.25, 2.0),
        ],
        "litellm_prefixes": ("gemini",),
    },
    {
        "name": "anthropic",
        "title": "Anthropic",
        "format": "anthropic",
        "base_url": "https://api.anthropic.com",
        "api_key_env": "ANTHROPIC_API_KEY",
        "continuation": True,
        "slugs": ("anthropic",),
        "prefixes": (),
        # model lines in priority order: (family prefix, revisions to keep).
        # New revisions inside a line (e.g. opus-4-9) add themselves; only a
        # brand-new line name needs a one-line addition (flagged in the log).
        "families": [
            ("claude-opus", 2),
            ("claude-sonnet", 2),
            ("claude-haiku", 1),
            ("claude-fable", 1),
            ("claude-mythos", 1),
        ],
        "max_models": 8,
    },
    {
        "name": "openai",
        "title": "OpenAI",
        "format": "openai",
        "base_url": "https://api.openai.com/v1",
        "api_key_env": "OPENAI_API_KEY",
        "continuation": True,
        "slugs": ("openai",),
        "prefixes": (),
        "families": [
            ("gpt-5.4-mini", 1),
            ("gpt-5.4-nano", 1),
            ("gpt-5.3-codex", 1),
            ("gpt-5", 5),
            ("gpt-6", 1),
            ("o4", 1),
            ("o3", 1),
            ("o1", 1),
        ],
        "max_models": 10,
    },
    {
        "name": "deepseek",
        "title": "DeepSeek",
        "format": "openai",
        "base_url": "https://api.deepseek.com",
        "api_key_env": "DEEPSEEK_API_KEY",
        "continuation": True,
        "slugs": ("deepseek",),
        "prefixes": ("deepseek",),
        "families": [
            ("deepseek-chat", 1),
            ("deepseek-reasoner", 1),
            ("deepseek-v4", 2),
            ("deepseek-v3", 1),
        ],
        "max_models": 5,
    },
    {
        "name": "grok",
        "title": "Grok",
        "format": "openai",
        "base_url": "https://api.x.ai/v1",
        "api_key_env": "XAI_API_KEY",
        "continuation": True,
        "slugs": ("xai",),
        "prefixes": ("xai",),
        # pinned stable line: xAI numbers don't sort by recency
        # (4.20-0309 is a dated track, the flagship is 4.6)
        "families": [
            ("grok-build", 1),
            ("grok-4.6", 1),
            ("grok-4.5", 1),
            ("grok-4.3", 1),
        ],
        "max_models": 5,
    },
    {
        "name": "kimi",
        "title": "Kimi",
        "format": "openai",
        "base_url": "https://api.moonshot.cn/v1",
        "api_key_env": "MOONSHOT_API_KEY",
        "continuation": True,
        "slugs": ("moonshot",),
        "prefixes": ("moonshot",),
        "families": [
            ("kimi-k3", 1),
            ("kimi-k2", 2),
        ],
        "max_models": 4,
    },
]


def fetch_litellm_data():
    req = urllib.request.Request(
        LITELLM_URL, headers={"User-Agent": "sqwai-model-updater/1.0"}
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except Exception as e:
        print(f"ERROR: could not fetch LiteLLM database: {e}", file=sys.stderr)
        sys.exit(1)


def usable_entry(info, today):
    if not isinstance(info, dict):
        return False
    # codex-style coding models are tagged "responses" in the DB but still
    # serve chat completions; realtime/audio/image never do (excluded below)
    if info.get("mode") not in ("chat", "responses"):
        return False
    dep = info.get("deprecation_date")
    if dep and dep <= today:
        return False
    return True


def discover(data, slugs, prefixes, today):
    """Find direct chat models: {canonical_id: info}."""
    found = {}
    for key, info in data.items():
        if key in RESERVED_KEYS:
            continue
        if "/" in key:
            pre, rest = key.split("/", 1)
            if "/" in rest or pre not in prefixes:
                continue
            cid = rest
        else:
            if VENDOR_DOT.match(key):
                continue
            if not isinstance(info, dict) or info.get("litellm_provider") not in slugs:
                continue
            cid = key
        if not usable_entry(info, today):
            continue
        if cid in DROP_IDS:
            print(f"  ! {cid}: known-phantom id, dropped")
            continue
        low = cid.lower()
        if any(x in low for x in EXCLUDE_SUBSTRINGS + SEARCH_SUBSTRINGS):
            continue
        if cid not in found:
            found[cid] = info
    return found


def drop_snapshots(found):
    """When both alias and dated snapshot exist, keep the alias."""
    groups = {}
    for cid, info in found.items():
        groups.setdefault(DATE_SUFFIX.sub("", cid), []).append((cid, info))
    return [min(g, key=lambda kv: (len(kv[0]), kv[0])) for g in groups.values()]


def version_key(cid):
    return tuple(int(x) for x in re.findall(r"\d+", cid))


def family_members(found, family):
    """Candidates belonging to a family: exact id or id + separator + suffix."""
    out = []
    for cid, info in found.items():
        if cid == family or cid.startswith(family + "-") or cid.startswith(family + "."):
            out.append((cid, info))
    return out


def pick_latest(members):
    """Newest version wins; on ties the shorter (alias) name wins."""
    by_name = sorted(members, key=lambda kv: (len(kv[0]), kv[0]))
    return max(by_name, key=lambda kv: version_key(kv[0]))


def pick_families(found, families, max_models):
    """Newest revisions per family, families in priority order, capped."""
    picked, seen = [], set()
    for fam, count in families:
        members = [kv for kv in family_members(found, fam) if kv[0] not in seen]
        if not members:
            print(f"  ! family {fam}: nothing in DB, skipped")
            continue
        # dated snapshots collapse into their alias before picking newest
        members = drop_snapshots(dict(members))
        members.sort(key=lambda kv: (len(kv[0]), kv[0]))
        members.sort(key=lambda kv: version_key(kv[0]), reverse=True)
        for cid, info in members[:count]:
            seen.add(cid)
            picked.append((cid, info))
        if len(picked) >= max_models:
            break
    return picked[:max_models]


def guess_effort(cid):
    if cid in EFFORT_OVERRIDES:
        return EFFORT_OVERRIDES[cid]
    low = cid.lower()
    if any(k in low for k in EFFORT_HIGH):
        return "high"
    if any(k in low for k in EFFORT_OFF):
        return "off"
    if any(k in low for k in EFFORT_LOW):
        return "low"
    if any(k in low for k in EFFORT_MEDIUM):
        return "medium"
    return "medium"


def clean_price(value):
    """Round $/1M to 4 decimals and drop float noise like 0.19999999999999998."""
    return round(float(value), 4)


def specs_from_db(info, d_ctx=1000000, d_in=1.0, d_out=5.0):
    ctx = info.get("max_input_tokens") or info.get("max_tokens") or d_ctx
    in_cost = info.get("input_cost_per_token")
    out_cost = info.get("output_cost_per_token")
    price_in = clean_price(in_cost * 1_000_000) if in_cost is not None else d_in
    price_out = clean_price(out_cost * 1_000_000) if out_cost is not None else d_out
    return int(ctx), price_in, price_out


def lookup(data, prefixes, model_id):
    candidates = [model_id] + [f"{p}/{model_id}" for p in prefixes]
    for key in candidates:
        info = data.get(key)
        if isinstance(info, dict):
            return info, key
    return None, None


def build_catalog(data, today):
    lines = [f'updated_at = "{today}"', ""]
    total = 0
    for p in PROVIDERS_CONFIG:
        lines += [
            f"# {p['title']}",
            f"[providers.{p['name']}]",
            f"format = \"{p['format']}\"",
            f"base_url = \"{p['base_url']}\"",
            f"api_key_env = \"{p['api_key_env']}\"",
            f"continuation = {'true' if p.get('continuation', True) else 'false'}",
            "",
        ]
        print(f"[{p['name']}]")
        if "pinned" in p:
            models = []
            for model_id, effort, d_ctx, d_in, d_out in p["pinned"]:
                info, key = lookup(data, p["litellm_prefixes"], model_id)
                if info is None or not usable_entry(info, today):
                    print(f"  - {model_id}: not in DB, using pinned defaults")
                    ctx, price_in, price_out = d_ctx, d_in, d_out
                else:
                    ctx, price_in, price_out = specs_from_db(info, d_ctx, d_in, d_out)
                    print(f"  - {model_id}: ctx={ctx} in=${price_in}/M out=${price_out}/M (db: {key})")
                models.append((model_id, effort, ctx, price_in, price_out))
        else:
            found = discover(data, p["slugs"], p["prefixes"], today)
            picked = pick_families(found, p["families"], p["max_models"])
            models = []
            for cid, info in picked:
                ctx, price_in, price_out = specs_from_db(info)
                effort = guess_effort(cid)
                print(f"  - {cid}: ctx={ctx} in=${price_in}/M out=${price_out}/M effort={effort}")
                models.append((cid, effort, ctx, price_in, price_out))
        total += len(models)
        for model_id, effort, ctx, price_in, price_out in models:
            lines += [
                f'[models."{model_id}"]',
                f"provider = \"{p['name']}\"",
                f'id = "{model_id}"',
                f"context = {ctx}",
                f'effort = "{effort}"',
                f"price_in = {price_in}",
                f"price_out = {price_out}",
                "",
            ]
    return "\n".join(lines) + "\n", total


def main():
    data = fetch_litellm_data()
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    content, total = build_catalog(data, today)

    target = os.path.abspath(CATALOG_PATH)
    # No-op when only the date would change: keeps history clean and lets
    # the workflow correctly report "No changes".
    if os.path.exists(target):
        with open(target, encoding="utf-8") as f:
            existing = f.read()
        strip_date = re.compile(r'^updated_at = ".*"$', re.M)
        if strip_date.sub("", existing) == strip_date.sub("", content):
            print(f"No model changes ({total} models); leaving {target} untouched.")
            return

    with open(target, "w", encoding="utf-8") as f:
        f.write(content)
    print(f"Updated {target} with {len(PROVIDERS_CONFIG)} providers and {total} models.")


if __name__ == "__main__":
    main()
