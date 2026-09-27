#!/usr/bin/env python3
"""
Direct model discovery: ask each provider's own list-models API what exists.

Why: the LiteLLM database lags and invents phantoms (it once listed
`gpt-6-astra`, which was never deliverable). The provider itself is the
source of truth for *existence*; specs still resolve through the overlay
and the LiteLLM DB downstream in update_providers.py.

Reads API keys from the same env names the app uses (OPENAI_API_KEY, ...).
A missing/empty key skips that provider with a warning — never fatal, so a
provider without a key (or with a failing API) degrades to LiteLLM-only
instead of going red.

Output: discovery/<name>.json per reached provider: {"ids": [...]}.
Stdlib only.
"""

import json
import os
import sys
import urllib.request

OUT_DIR = os.path.join(os.path.dirname(__file__), "..", "..", "discovery")


def get(url, headers=None, timeout=20):
    req = urllib.request.Request(url, headers=headers or {"User-Agent": "sqwai-discovery/1.0"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def openai_style(base_url, key):
    """OpenAI-compatible GET <base>/models with a Bearer key."""
    data = get(
        base_url.rstrip("/") + "/models",
        headers={"Authorization": f"Bearer {key}", "User-Agent": "sqwai-discovery/1.0"},
    )
    return [m["id"] for m in data.get("data", []) if isinstance(m, dict) and m.get("id")]


def fetch_gemini(key):
    ids, page_token = [], None
    while True:
        url = (
            "https://generativelanguage.googleapis.com/v1beta/models"
            f"?pageSize=1000&key={key}"
        )
        if page_token:
            url += f"&pageToken={page_token}"
        data = get(url)
        for m in data.get("models", []):
            name = m.get("name", "")
            if name.startswith("models/"):
                ids.append(name[len("models/"):])
        page_token = data.get("nextPageToken")
        if not page_token:
            return ids


def fetch_anthropic(key):
    data = get(
        "https://api.anthropic.com/v1/models?limit=1000",
        headers={
            "x-api-key": key,
            "anthropic-version": "2023-06-01",
            "User-Agent": "sqwai-discovery/1.0",
        },
    )
    return [m["id"] for m in data.get("data", []) if isinstance(m, dict) and m.get("id")]


PROVIDERS = [
    ("gemini", "GEMINI_API_KEY", fetch_gemini),
    ("anthropic", "ANTHROPIC_API_KEY", fetch_anthropic),
    ("openai", "OPENAI_API_KEY", lambda k: openai_style("https://api.openai.com/v1", k)),
    ("deepseek", "DEEPSEEK_API_KEY", lambda k: openai_style("https://api.deepseek.com/v1", k)),
    ("kimi", "MOONSHOT_API_KEY", lambda k: openai_style("https://api.moonshot.ai/v1", k)),
    ("grok", "XAI_API_KEY", lambda k: openai_style("https://api.x.ai/v1", k)),
]


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    reached = 0
    for name, env, fetch in PROVIDERS:
        key = os.environ.get(env, "").strip()
        if not key:
            print(f"[{name}] no {env}, skipped (LiteLLM-only)")
            continue
        try:
            ids = sorted(set(fetch(key)))
        except Exception as e:
            print(f"[{name}] direct fetch failed ({e}), LiteLLM-only", file=sys.stderr)
            continue
        with open(os.path.join(OUT_DIR, f"{name}.json"), "w", encoding="utf-8") as f:
            json.dump({"ids": ids}, f)
        print(f"[{name}] {len(ids)} ids")
        reached += 1
    print(f"direct discovery reached {reached}/{len(PROVIDERS)} providers")


if __name__ == "__main__":
    main()
