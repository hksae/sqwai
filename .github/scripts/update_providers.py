#!/usr/bin/env python3
"""
Sync builtin_providers.toml with latest model specifications and pricing.

Source of truth for specs/prices: LiteLLM model_prices_and_context_window.json
(plus official provider docs as fallback for models missing from that DB,
e.g. direct xAI models).

Policy (agreed):
- Legacy/retired IDs are REPLACED, not kept (grok-2, grok-beta, moonshot-v1,
  kimi-latest, gpt-4o, o1/o3-mini, claude-3-*).
- Gemini list is pinned by the repo owner; other providers are auto-resolved:
  the script looks up each tracked alias in the DB (bare key, then
  "<provider>/key") and refreshes context window + prices.
- Entries deprecated in the DB (deprecation_date in the past) are dropped.
- Direct chat models unknown to the tracked list are reported to stdout as
  CANDIDATES so the maintainer can see what to add next.
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
    "embedding", "search-preview", "diarize", "vision-preview",
)

# (tracked_alias, effort, fallback_ctx, fallback_in, fallback_out)
PROVIDERS_CONFIG = [
    {
        "name": "gemini",
        "title": "Gemini",
        "format": "openai",
        "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
        "api_key_env": "GEMINI_API_KEY",
        "continuation": True,
        "pinned": True,  # owner-curated list, only specs/prices refresh from DB
        "litellm_prefixes": ("gemini/",),
        "models": [
            ("gemini-3.1-pro-preview", "high", 1048576, 2.0, 12.0),
            ("gemini-3.8-flash", "medium", 1048576, 0.75, 3.75),
            ("gemini-3.7-flash", "medium", 1048576, 0.75, 3.75),
            ("gemini-3.6-flash", "medium", 1048576, 0.5, 3.0),
            ("gemini-3.5-flash", "medium", 1048576, 0.5, 3.0),
            ("gemini-3.5-flash-lite", "off", 1048576, 0.3, 2.5),
            ("gemini-3.1-flash-lite", "off", 1048576, 0.25, 2.0),
        ],
    },
    {
        "name": "anthropic",
        "title": "Anthropic",
        "format": "anthropic",
        "base_url": "https://api.anthropic.com",
        "api_key_env": "ANTHROPIC_API_KEY",
        "continuation": True,
        "litellm_prefixes": ("anthropic/",),
        "models": [
            ("claude-opus-4-6", "high", 1000000, 5.0, 25.0),
            ("claude-opus-4-5", "high", 200000, 5.0, 25.0),
            ("claude-sonnet-4-5", "high", 200000, 3.0, 15.0),
            ("claude-sonnet-5", "medium", 1000000, 2.0, 10.0),
            ("claude-haiku-4-5", "medium", 200000, 0.8, 4.0),
        ],
    },
    {
        "name": "openai",
        "title": "OpenAI",
        "format": "openai",
        "base_url": "https://api.openai.com/v1",
        "api_key_env": "OPENAI_API_KEY",
        "continuation": True,
        "litellm_prefixes": (),
        "models": [
            ("gpt-5.5", "high", 400000, 2.5, 15.0),
            ("gpt-5.4", "high", 1048576, 2.5, 15.0),
            ("gpt-5.4-mini", "off", 1048576, 0.5, 3.0),
            ("gpt-5.3-codex", "high", 400000, 1.75, 14.0),
            ("o4-mini", "medium", 200000, 1.1, 4.4),
        ],
    },
    {
        "name": "deepseek",
        "title": "DeepSeek",
        "format": "openai",
        "base_url": "https://api.deepseek.com",
        "api_key_env": "DEEPSEEK_API_KEY",
        "continuation": True,
        "litellm_prefixes": ("deepseek/",),
        "models": [
            ("deepseek-chat", "off", 128000, 0.27, 1.1),
            ("deepseek-reasoner", "high", 128000, 0.55, 2.19),
        ],
    },
    {
        "name": "grok",
        "title": "Grok",
        "format": "openai",
        "base_url": "https://api.x.ai/v1",
        "api_key_env": "XAI_API_KEY",
        "continuation": True,
        "litellm_prefixes": ("xai/",),
        # Current xAI lineup per docs.x.ai (Sep 2026). The LiteLLM DB mostly
        # carries xAI models via third-party routes, so official docs values
        # are the fallback here.
        "models": [
            ("grok-4.6", "high", 500000, 2.0, 6.0),
            ("grok-4.5", "high", 500000, 2.0, 6.0),
            ("grok-4.3", "medium", 1000000, 1.25, 2.5),
            ("grok-build-0.1", "high", 256000, 1.0, 2.0),
        ],
    },
    {
        "name": "kimi",
        "title": "Kimi",
        "format": "openai",
        "base_url": "https://api.moonshot.cn/v1",
        "api_key_env": "MOONSHOT_API_KEY",
        "continuation": True,
        "litellm_prefixes": ("moonshot/",),
        "models": [
            ("kimi-k3", "high", 1000000, 2.0, 8.0),
            ("kimi-k2.6", "medium", 256000, 1.0, 4.0),
            ("kimi-k2.7-code", "high", 256000, 1.0, 4.0),
        ],
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


def lookup(data, prefixes, model_id):
    """Return (info, matched_key) for a model id or None."""
    candidates = [model_id] + [f"{p}{model_id}" for p in prefixes]
    for key in candidates:
        info = data.get(key)
        if isinstance(info, dict):
            return info, key
    return None, None


def is_deprecated(info, today):
    dep = info.get("deprecation_date") if isinstance(info, dict) else None
    return bool(dep) and dep <= today


def clean_price(value):
    """Round $/1M to 4 decimals and drop float noise like 0.19999999999999998."""
    return round(float(value), 4)


def resolve_model(data, prefixes, model_id, effort, d_ctx, d_in, d_out, today):
    info, key = lookup(data, prefixes, model_id)
    if info is None:
        print(f"  - {model_id}: not in LiteLLM DB, using fallback defaults")
        return d_ctx, d_in, d_out
    if is_deprecated(info, today):
        print(f"  - {model_id}: DEPRECATED in DB ({info.get('deprecation_date')}), keeping with fallback")
        return d_ctx, d_in, d_out
    ctx = info.get("max_input_tokens") or info.get("max_tokens") or d_ctx
    in_cost = info.get("input_cost_per_token")
    out_cost = info.get("output_cost_per_token")
    price_in = clean_price(in_cost * 1_000_000) if in_cost is not None else d_in
    price_out = clean_price(out_cost * 1_000_000) if out_cost is not None else d_out
    print(f"  - {model_id}: ctx={ctx} in=${price_in}/M out=${price_out}/M (db: {key})")
    return int(ctx), price_in, price_out


def report_candidates(data, today):
    """List direct chat models in the DB that we do NOT track (visibility for maintainer)."""
    tracked = set()
    for p in PROVIDERS_CONFIG:
        for m in p["models"]:
            tracked.add(m[0])
            for prefix in p["litellm_prefixes"]:
                tracked.add(f"{prefix}{m[0]}")
    vendor_prefix = re.compile(r"^[a-z0-9_]+\.")
    interesting = []
    for key, info in data.items():
        if key in RESERVED_KEYS or not isinstance(info, dict):
            continue
        if "/" in key:  # third-party routes (azure_ai/, bedrock/, ...) are out of scope
            continue
        if vendor_prefix.match(key):  # bedrock-style (anthropic.claude-..., amazon....)
            continue
        if info.get("mode") != "chat":
            continue
        low = key.lower()
        if any(x in low for x in EXCLUDE_SUBSTRINGS):
            continue
        if is_deprecated(info, today):
            continue
        if key in tracked:
            continue
        interesting.append(key)
    interesting.sort()
    if interesting:
        print(f"\nUntracked direct chat models in DB ({len(interesting)}), consider adding:")
        for key in interesting[:30]:
            print(f"    ? {key}")


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
        for model_id, effort, d_ctx, d_in, d_out in p["models"]:
            total += 1
            ctx, price_in, price_out = resolve_model(
                data, p["litellm_prefixes"], model_id, effort, d_ctx, d_in, d_out, today
            )
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
    report_candidates(data, today)

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
