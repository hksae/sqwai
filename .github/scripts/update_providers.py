#!/usr/bin/env python3
"""
Sync builtin_providers.toml with latest model specifications and pricing from LiteLLM database.
Runs in GitHub Actions weekly or on-demand via workflow_dispatch.
"""

import datetime
import json
import os
import sys
import urllib.request

LITELLM_URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
CATALOG_PATH = os.path.join(os.path.dirname(__file__), "..", "..", "builtin_providers.toml")

# Target Gemini models to track
TRACKED_GEMINI_MODELS = [
    ("gemini-3.1-pro-preview", "high"),
    ("gemini-3.8-flash", "medium"),
    ("gemini-3.7-flash", "medium"),
    ("gemini-3.6-flash", "medium"),
    ("gemini-3.5-flash", "medium"),
    ("gemini-3.5-flash-lite", "off"),
    ("gemini-3.1-flash-lite", "off"),
]

def fetch_litellm_data():
    req = urllib.request.Request(
        LITELLM_URL,
        headers={"User-Agent": "sqwai-model-updater/1.0"}
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except Exception as e:
        print(f"Warning: could not fetch LiteLLM database: {e}", file=sys.stderr)
        return {}

def update_catalog():
    data = fetch_litellm_data()
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")

    output_lines = [
        f'updated_at = "{today}"',
        "",
        "[providers.gemini]",
        'format = "openai"',
        'base_url = "https://generativelanguage.googleapis.com/v1beta/openai"',
        'api_key_env = "GEMINI_API_KEY"',
        "continuation = true",
        ""
    ]

    for model_id, default_effort in TRACKED_GEMINI_MODELS:
        # Check LiteLLM keys
        # Could be "gemini/gemini-3.7-flash" or "gemini-3.7-flash"
        info = (
            data.get(f"gemini/{model_id}")
            or data.get(model_id)
            or {}
        )

        max_input = info.get("max_input_tokens") or 1048576
        input_cost_per_token = info.get("input_cost_per_token")
        output_cost_per_token = info.get("output_cost_per_token")

        # Convert to $ per 1M tokens
        price_in = (input_cost_per_token * 1_000_000) if input_cost_per_token is not None else None
        price_out = (output_cost_per_token * 1_000_000) if output_cost_per_token is not None else None

        # Fallback defaults if not yet in LiteLLM DB
        if price_in is None:
            if "flash-lite" in model_id:
                price_in = 0.25
                price_out = 2.0
            elif "flash" in model_id:
                price_in = 0.75
                price_out = 3.75
            else:
                price_in = 2.0
                price_out = 12.0

        output_lines.extend([
            f'[models."{model_id}"]',
            'provider = "gemini"',
            f'id = "{model_id}"',
            f"context = {max_input}",
            f'effort = "{default_effort}"',
            f"price_in = {price_in}",
            f"price_out = {price_out}",
            ""
        ])

    target_file = os.path.abspath(CATALOG_PATH)
    with open(target_file, "w", encoding="utf-8") as f:
        f.write("\n".join(output_lines))
    print(f"Updated {target_file} with {len(TRACKED_GEMINI_MODELS)} models.")

if __name__ == "__main__":
    update_catalog()
