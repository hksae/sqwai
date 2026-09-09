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

# Providers and their tracked models: (model_id, default_effort, default_ctx, default_price_in, default_price_out)
PROVIDERS_CONFIG = [
    {
        "name": "gemini",
        "format": "openai",
        "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
        "api_key_env": "GEMINI_API_KEY",
        "continuation": True,
        "models": [
            ("gemini-3.1-pro-preview", "high", 1048576, 2.0, 12.0),
            ("gemini-3.8-flash", "medium", 1048576, 0.75, 3.75),
            ("gemini-3.7-flash", "medium", 1048576, 0.75, 3.75),
            ("gemini-3.6-flash", "medium", 1048576, 0.5, 3.0),
            ("gemini-3.5-flash", "medium", 1048576, 0.5, 3.0),
            ("gemini-3.5-flash-lite", "off", 1048576, 0.3, 2.5),
            ("gemini-3.1-flash-lite", "off", 1048576, 0.25, 2.0),
        ]
    },
    {
        "name": "anthropic",
        "format": "anthropic",
        "base_url": "https://api.anthropic.com",
        "api_key_env": "ANTHROPIC_API_KEY",
        "continuation": True,
        "models": [
            ("claude-3-7-sonnet-20250219", "high", 200000, 3.0, 15.0),
            ("claude-3-5-sonnet-20241022", "high", 200000, 3.0, 15.0),
            ("claude-3-5-haiku-20241022", "medium", 200000, 0.8, 4.0),
            ("claude-3-opus-20240229", "high", 200000, 15.0, 75.0),
        ]
    },
    {
        "name": "openai",
        "format": "openai",
        "base_url": "https://api.openai.com/v1",
        "api_key_env": "OPENAI_API_KEY",
        "continuation": True,
        "models": [
            ("gpt-4o", "off", 128000, 2.5, 10.0),
            ("gpt-4o-mini", "off", 128000, 0.15, 0.60),
            ("o1", "high", 200000, 15.0, 60.0),
            ("o1-mini", "medium", 128000, 1.1, 4.40),
            ("o3-mini", "medium", 200000, 1.1, 4.40),
        ]
    },
    {
        "name": "deepseek",
        "format": "openai",
        "base_url": "https://api.deepseek.com",
        "api_key_env": "DEEPSEEK_API_KEY",
        "continuation": True,
        "models": [
            ("deepseek-chat", "off", 64000, 0.27, 1.10),
            ("deepseek-reasoner", "high", 64000, 0.55, 2.19),
        ]
    },
    {
        "name": "grok",
        "format": "openai",
        "base_url": "https://api.x.ai/v1",
        "api_key_env": "XAI_API_KEY",
        "continuation": True,
        "models": [
            ("grok-2-1212", "off", 131072, 2.0, 10.0),
            ("grok-2-vision-1212", "off", 32768, 2.0, 10.0),
            ("grok-beta", "off", 131072, 5.0, 15.0),
        ]
    },
    {
        "name": "kimi",
        "format": "openai",
        "base_url": "https://api.moonshot.cn/v1",
        "api_key_env": "MOONSHOT_API_KEY",
        "continuation": True,
        "models": [
            ("moonshot-v1-8k", "off", 8192, 1.70, 1.70),
            ("moonshot-v1-32k", "off", 32768, 3.40, 3.40),
            ("moonshot-v1-128k", "off", 128000, 8.50, 8.50),
            ("kimi-latest", "off", 128000, 8.50, 8.50),
        ]
    },
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

    output_lines = [f'updated_at = "{today}"', ""]
    total_models = 0

    for provider in PROVIDERS_CONFIG:
        p_name = provider["name"]
        p_format = provider["format"]
        p_url = provider["base_url"]
        p_env = provider["api_key_env"]
        p_cont = "true" if provider.get("continuation", True) else "false"

        output_lines.extend([
            f"# {p_name.capitalize()}",
            f"[providers.{p_name}]",
            f'format = "{p_format}"',
            f'base_url = "{p_url}"',
            f'api_key_env = "{p_env}"',
            f"continuation = {p_cont}",
            ""
        ])

        for model_id, default_effort, default_ctx, default_in, default_out in provider["models"]:
            total_models += 1
            # Search in LiteLLM DB under various aliases
            info = (
                data.get(f"{p_name}/{model_id}")
                or data.get(model_id)
                or data.get(f"xai/{model_id}")
                or data.get(f"moonshot/{model_id}")
                or {}
            )

            max_input = info.get("max_input_tokens") or default_ctx
            input_cost = info.get("input_cost_per_token")
            output_cost = info.get("output_cost_per_token")

            price_in = (input_cost * 1_000_000) if input_cost is not None else default_in
            price_out = (output_cost * 1_000_000) if output_cost is not None else default_out

            output_lines.extend([
                f'[models."{model_id}"]',
                f'provider = "{p_name}"',
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
    print(f"Updated {target_file} with {len(PROVIDERS_CONFIG)} providers and {total_models} models.")

if __name__ == "__main__":
    update_catalog()
