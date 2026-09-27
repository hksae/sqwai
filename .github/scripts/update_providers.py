#!/usr/bin/env python3
"""
Sync builtin_providers.toml with latest model specifications.

Source of truth for specs: LiteLLM model_prices_and_context_window.json.

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
- With --discovery-dir, direct lists from discover_providers.py union in
  ids the DB missed (specs: provider_overlay.toml -> LiteLLM DB; ids with
  neither become report candidates, never heuristic guesses). Ids the
  catalog shipped but the run drops are
  reported, never silently re-added; removals always need human review.
- Freshness is a monotonic `serial`, not the date: when models change, the
  new file carries existing serial + 1 (the client refuses replays of older
  serials, so the number must never go backwards or be reset by hand).
- If nothing but the stamps changed, the file is left untouched (keeps the
  old updated_at, so git sees no diff and no empty commit is made).
"""

import argparse
import datetime
import json
import os
import re
import sys
import urllib.request

try:
    import tomllib
except ImportError:  # overlay needs 3.11+; CI runs 3.12, older stays DB-only
    tomllib = None

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

# Freshness stamps: serial is the rollback floor the client enforces,
# updated_at is informational. Both are ignored when deciding whether
# the models actually changed.
SERIAL_RE = re.compile(r"^serial = (\d+)$", re.M)
STAMP_RE = re.compile(r'^(serial = \d+|updated_at = ".*")$', re.M)

# Confirmed-phantom API IDs: present in the LiteLLM DB but not real models
# (e.g. bare gpt-5.6 — only luna/terra/sol/cyber exist). Logged when dropped.
DROP_IDS = {"gpt-5.6"}

# Extra substrings that disqualify a model for coding/chat use in sqwai.
# (generic audio/image/... live in EXCLUDE_SUBSTRINGS; these are search APIs.)
SEARCH_SUBSTRINGS = ("search",)

EFFORT_VALUES = ("off", "low", "medium", "high")

OVERLAY_PATH = os.path.join(os.path.dirname(__file__), "provider_overlay.toml")

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
        # owner-pinned list; only specs refresh from the DB
        "pinned": [
            ("gemini-3.1-pro-preview", "high", 1048576),
            ("gemini-3.8-flash", "medium", 1048576),
            ("gemini-3.7-flash", "medium", 1048576),
            ("gemini-3.6-flash", "medium", 1048576),
            ("gemini-3.5-flash", "medium", 1048576),
            ("gemini-3.5-flash-lite", "off", 1048576),
            ("gemini-3.1-flash-lite", "off", 1048576),
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
        "base_url": "https://api.moonshot.ai/v1",
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


def specs_from_db(info, d_ctx=1000000):
    ctx = info.get("max_input_tokens") or info.get("max_tokens") or d_ctx
    return int(ctx)


def lookup(data, prefixes, model_id):
    candidates = [model_id] + [f"{p}/{model_id}" for p in prefixes]
    for key in candidates:
        info = data.get(key)
        if isinstance(info, dict):
            return info, key
    return None, None


def load_discovery(discovery_dir):
    """{provider: [ids]} from discover_providers.py; missing dir = {}."""
    out = {}
    if not discovery_dir or not os.path.isdir(discovery_dir):
        return out
    for name in [p["name"] for p in PROVIDERS_CONFIG]:
        path = os.path.join(discovery_dir, f"{name}.json")
        if not os.path.exists(path):
            continue
        try:
            with open(path, encoding="utf-8") as f:
                ids = json.load(f).get("ids", [])
            out[name] = [i for i in ids if isinstance(i, str) and i]
        except Exception as e:
            print(f"  ! discovery file for {name} unreadable ({e}), LiteLLM-only")
    return out


def load_overlay():
    """{provider: {id: (context, effort)}}; bad entries warn and drop."""
    if tomllib is None or not os.path.exists(OVERLAY_PATH):
        return {}
    try:
        with open(OVERLAY_PATH, "rb") as f:
            raw = tomllib.load(f)
    except Exception as e:
        print(f"  ! overlay unreadable ({e}), ignored")
        return {}
    out = {}
    for provider, table in raw.items():
        if not isinstance(table, dict):
            continue
        for mid, spec in table.items():
            if not isinstance(spec, dict):
                continue
            try:
                ctx = int(spec["context"])
                effort = str(spec["effort"])
            except (KeyError, ValueError, TypeError):
                print(f"  ! overlay {provider}/{mid}: bad spec, ignored")
                continue
            if ctx <= 0 or effort not in EFFORT_VALUES:
                print(f"  ! overlay {provider}/{mid}: bad spec, ignored")
                continue
            out.setdefault(provider, {})[mid] = (ctx, effort)
    return out


def catalog_model_ids(text):
    """{provider: {ids}} parsed from a catalog file (header + provider lines)."""
    out, current = {}, None
    for line in text.splitlines():
        m = re.match(r'^\[models\."([^"]+)"\]$', line.strip())
        if m:
            current = m.group(1)
            continue
        m = re.match(r'^provider\s*=\s*"([^"]+)"$', line.strip())
        if m and current:
            out.setdefault(m.group(1), set()).add(current)
            current = None
    return out


def direct_extras(p, have, data, discovered, overlay, today):
    """Discovered ids the LiteLLM path missed: (id, effort, ctx, source).

    Auto-add is gated on a spec source — overlay entry or usable LiteLLM
    record. A bare direct id proves existence, not suitability: Google and
    OpenAI list test harnesses (aqa), music/video/image models (lyria, veo,
    sora), moderation and robotics endpoints next to chat ones, and shape
    alone cannot tell them apart. Gated-out ids are returned as candidates
    for the report (a curator pins the real ones via the overlay), never
    added with heuristic specs. Anything looking non-chat (exclusions,
    snapshots, known phantoms) is skipped with a log line, same policy as
    the DB path.
    """
    extras, candidates = [], []
    prefixes = tuple(p.get("litellm_prefixes", ()) or p.get("prefixes", ()))
    for did in discovered:
        if did in have or did in DROP_IDS:
            continue
        low = did.lower()
        if any(x in low for x in EXCLUDE_SUBSTRINGS + SEARCH_SUBSTRINGS):
            print(f"  - {did}: direct-only but excluded substring, skipped")
            continue
        if DATE_SUFFIX.search(did):
            print(f"  - {did}: direct-only dated snapshot, skipped")
            continue
        if did in overlay.get(p["name"], {}):
            ctx, effort = overlay[p["name"]][did]
            source = "overlay"
        else:
            info, _ = lookup(data, prefixes, did)
            if info is not None and usable_entry(info, today):
                ctx, effort = specs_from_db(info), guess_effort(did)
                source = "litellm"
            else:
                candidates.append(did)
                print(f"  - {did}: direct-only, no spec source (candidate, not added)")
                continue
        extras.append((did, effort, ctx, source))
    return extras, candidates


def build_catalog(data, today, serial, discovery=None, overlay=None, report=None):
    lines = [
        "# Monotonic freshness stamp, bumped by .github/scripts/update_providers.py on",
        "# every content change (never by hand). The client compares serials, not",
        "# dates: a replayed old file never wins, and missing serial reads as 0.",
        f"serial = {serial}",
        f'updated_at = "{today}"',
        "",
    ]
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
            for model_id, effort, d_ctx in p["pinned"]:
                info, key = lookup(data, p["litellm_prefixes"], model_id)
                if info is None or not usable_entry(info, today):
                    print(f"  - {model_id}: not in DB, using pinned defaults")
                    ctx = d_ctx
                else:
                    ctx = specs_from_db(info, d_ctx)
                    print(f"  - {model_id}: ctx={ctx} (db: {key})")
                models.append((model_id, effort, ctx))
        else:
            found = discover(data, p["slugs"], p["prefixes"], today)
            picked = pick_families(found, p["families"], p["max_models"])
            models = []
            for cid, info in picked:
                ctx = specs_from_db(info)
                effort = guess_effort(cid)
                print(f"  - {cid}: ctx={ctx} effort={effort}")
                models.append((cid, effort, ctx))
        total += len(models)
        sec = {"added": [], "candidates": [], "source": "litellm", "direct_ids": 0}
        discovered = (discovery or {}).get(p["name"])
        if discovered is not None:
            sec["source"] = f"direct ({len(discovered)} ids) + litellm"
            sec["direct_ids"] = len(discovered)
            have = {mid for mid, _, _ in models}
            added, candidates = direct_extras(
                p, have, data, discovered, overlay or {}, today
            )
            for did, effort, ctx, origin in added:
                models.append((did, effort, ctx))
                sec["added"].append(f"{did} (ctx={ctx} via {origin})")
                print(f"  + {did}: direct-only, ctx={ctx} via {origin}")
            sec["candidates"] = candidates
            total += len(sec["added"])
        if report is not None:
            report[p["name"]] = sec
        for model_id, effort, ctx in models:
            lines += [
                f'[models."{model_id}"]',
                f"provider = \"{p['name']}\"",
                f'id = "{model_id}"',
                f"context = {ctx}",
                f'effort = "{effort}"',
                "",
            ]
    return "\n".join(lines) + "\n", total


def write_sync_files(kind, lines):
    with open("catalog_change.txt", "w", encoding="utf-8") as f:
        f.write(kind + "\n")
    with open("catalog_report.md", "w", encoding="utf-8") as f:
        f.write("".join(lines))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--discovery-dir",
        default=None,
        help="directory with discovery/<provider>.json from discover_providers.py",
    )
    args = ap.parse_args()

    data = fetch_litellm_data()
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    discovery = load_discovery(args.discovery_dir)
    overlay = load_overlay()
    if discovery:
        print(f"direct discovery active for: {', '.join(sorted(discovery))}")
    report = {}

    target = os.path.abspath(CATALOG_PATH)
    existing = None
    if os.path.exists(target):
        with open(target, encoding="utf-8") as f:
            existing = f.read()
    m = SERIAL_RE.search(existing) if existing else None
    serial = (int(m.group(1)) + 1) if m else 1
    content, total = build_catalog(data, today, serial, discovery, overlay, report)

    # Change classification (stamps ignored): removals always need a human,
    # pure additions and spec tweaks auto-merge.
    old_ids = catalog_model_ids(existing) if existing else {}
    new_ids = catalog_model_ids(content)
    old_all = {i for ids in old_ids.values() for i in ids}
    new_all = {i for ids in new_ids.values() for i in ids}
    removed = sorted(old_all - new_all)
    stale = []
    for name, dids in discovery.items():
        dset = set(dids)
        gone = sorted(old_ids.get(name, set()) - dset - new_ids.get(name, set()))
        stale += [(name, i, "retired upstream") for i in gone]
        dropped = sorted((old_ids.get(name, set()) & dset) - new_ids.get(name, set()))
        stale += [(name, i, "still served but dropped by family caps") for i in dropped]

    md = [f"# Catalog sync — {today} (serial {serial})\n\n"]
    for p in PROVIDERS_CONFIG:
        sec = report.get(p["name"], {"added": [], "candidates": [], "source": "litellm", "direct_ids": 0})
        md.append(f"## {p['name']} — {sec['source']}\n")
        for a in sec["added"]:
            md.append(f"- added {a}\n")
        if not sec["added"]:
            md.append("- no direct-only additions\n")
        if sec["candidates"]:
            md.append(
                "- candidates (direct-listed, no spec source — "
                "pin the real ones in provider_overlay.toml):\n"
            )
            for c in sec["candidates"]:
                md.append(f"  - {c}\n")
    if stale:
        md.append("\n## Needs human review\n")
        for name, mid, why in stale:
            md.append(f"- {name}/{mid}: {why}\n")
    kind = "removal" if removed else "additive"
    md.append(f"\nChange kind: **{kind}**\n")

    # No-op when only the stamps would change: keeps history clean and lets
    # the workflow correctly report "No changes".
    if existing is not None and STAMP_RE.sub("", existing) == STAMP_RE.sub("", content):
        print(f"No model changes ({total} models); leaving {target} untouched.")
        write_sync_files("none", md)
        return

    with open(target, "w", encoding="utf-8") as f:
        f.write(content)
    print(f"Updated {target} with {len(PROVIDERS_CONFIG)} providers and {total} models (serial {serial}).")
    write_sync_files(kind, md)


if __name__ == "__main__":
    main()
