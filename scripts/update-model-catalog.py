#!/usr/bin/env python3
"""Refresh the compiled model catalog from models.dev.

`crates/core/src/ai/data/models.generated.json` is the catalog every build
ships (ADR-compiled-model-catalog-refreshes-from-models-dev). The pi port
embedded one snapshot of it and the generator behind that snapshot was not
ported; this script is the replacement.

    python3 scripts/update-model-catalog.py                 # fetch + rewrite
    python3 scripts/update-model-catalog.py --dry-run       # report only
    python3 scripts/update-model-catalog.py --source api.json

Review the diff, then commit the JSON.

What the source decides, for a model the catalog already lists under a
provider (same id): `name`, `reasoning`, `contextWindow`, `maxTokens`,
`input`, `cost`, and `thinkingLevelMap` — the ladder only while the source
names effort levels (a toggle-only or missing `reasoning_options` leaves the
catalog's own ladder alone), and never an all-zero price over a non-zero one
unless the id explicitly names a free variant (otherwise the source's zero
means subscription or unknown, and the usage display reads the catalog's
reference price).

What stays the catalog's, because the source does not know it: `api`,
`baseUrl`, `compat`, `headers` (the transport shape pi worked out per
provider), a level mapped to another value (`minimal` → `low` on Copilot
Claude) and the `off` entry — pi's "cannot turn thinking off" on Claude and
Kimi K3 is not a fact the source can take back.

A model the source lists and the catalog lacks is appended, inheriting the
transport fields from the existing model it most resembles: the same id
prefix (the grouping gateways route by — `anthropic/`, `gpt`) or the family
under a version bump in it (`k3.1-pro` after `k3`), narrowed to the same
ladder when that subset exists. A prefix the catalog splits across
transports, or one it has never seen, is pi's per-model knowledge the source
does not carry — such a model is skipped and noted for a hand entry — except
on the INHERIT_DOMINANT gateways, whose general-purpose transport serves any
vendor. Deprecated source models are not added, and neither are non-chat rows
(image, video, embedding, speech), date-stamped snapshots (the picker hides
them behind the moving alias), or the few ids in SKIP_ADDITIONS the catalog
keeps out on purpose. An existing entry leaves when the source retires it:
its provider's row typed realtime or marked deprecated, or a model the
provider no longer lists — where that listing is the provider's own catalog
(RETIRE_UNLISTED; a moving alias in KEEP_UNLISTED stays) or where any
provider's row marks that id realtime. Gateways, regional catalogs and plans
keep their other unlisted models — models.dev is incomplete about regional
and plan-specific variants — and hiding them is the user's call.

Providers match the source by baseUrl against its `api` first — what is
actually called wins over a shared name: Holt's `zai` is the coding
endpoint, models.dev's `zai` the general one — and by id second.
SOURCE_OVERRIDE lists the providers neither rule reaches. A source feeding
two catalog providers (the Alibaba token plan and its individual variant
share one listing) adds nothing to either, since the script cannot tell
which of the two serves a new model; REFRESH_ONLY providers take refreshes
only.

The document also carries `modelVendors`: the providers whose listing is
where a model lives — the makers, not the inference hosts that serve other
makers' models under limits of their own — each with the id prefixes of the
models that are its own, since a maker's plan also serves other makers'
models. When a listing under a provider the catalog does not know offers an
id the catalog carries elsewhere, the engine adopts that maker's record for
it, never a gateway's, whose rows carry partial ladders and their own
transport quirks. The list is written with the catalog so the engine reads
it rather than a copy of it.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
CATALOG = REPO / "crates/core/src/ai/data/models.generated.json"
SOURCE_URL = "https://models.dev/api.json"

LEVELS = ("minimal", "low", "medium", "high", "xhigh", "max")
# Levels the engine offers unless the map nulls them (core
# `get_supported_thinking_levels`); xhigh and max need a named mapping.
DEFAULT_ON = ("minimal", "low", "medium", "high")
# The catalog lists chat models only — text in, text out — like the snapshot
# it replaces; image, video, embedding, speech, transcription and reranking
# rows in the source are not models a Turn can run on.
NON_CHAT_TYPES = frozenset(
    {"image", "video", "embedding", "speech", "transcription", "reranking", "realtime"}
)
# Realtime rows cannot use any compiled chat transport. Other legacy rows
# stay: refreshing is not a catalog cleanup pass.
REMOVE_EXISTING_TYPES = frozenset({"realtime"})

SOURCE_OVERRIDE = {
    # baseUrl has no /v1; the source's has.
    "kimi-coding": "kimi-code-plan-cn",
    "together": "togetherai",
    "vercel-ai-gateway": "vercel",
    # Deployment-specific baseUrl (empty); the ids are OpenAI's.
    "azure-openai-responses": "azure",
}
REFRESH_ONLY = {
    # The source lists Claude, Grok and Llama under `azure` too, which this
    # provider's OpenAI Responses transport does not serve.
    "azure-openai-responses",
}
# Gateways that serve every model the catalog has no prefix sibling for through
# their one general-purpose transport, so a new vendor prefix may inherit the
# provider's dominant shape; everywhere else such a model is skipped.
INHERIT_DOMINANT = {
    # Every non-Anthropic model rides the OpenAI-compatible endpoint.
    "openrouter",
}
# Source models the catalog deliberately leaves out, for reasons the source
# does not express; the core goldens pin each one.
SKIP_ADDITIONS = {
    # Served only through inference profiles (the regional and global ids).
    ("amazon-bedrock", "anthropic.claude-opus-5"),
    # Retired or redundant dated snapshots; the moving alias stays.
    ("xai", "grok-4.20-0309-reasoning"),
    ("xai", "grok-4.20-0309-non-reasoning"),
    ("xai", "grok-4.20-multi-agent-0309"),
}
# Providers whose models.dev listing is the provider's own catalog: a model it
# no longer lists is retired there. Gateways, regional catalogs and plans are
# not — models.dev is incomplete about what they serve, and a plan's listing
# lags: a model it marks deprecated on one plan is merely absent from another.
RETIRE_UNLISTED = {
    "anthropic",
    "ant-ling",
    "baseten",
    "cerebras",
    "cloudflare-workers-ai",
    "deepseek",
    "fireworks",
    "google",
    "google-vertex",
    "groq",
    "huggingface",
    "minimax",
    "minimax-cn",
    "mistral",
    "moonshotai",
    "moonshotai-cn",
    "nvidia",
    "openai",
    "together",
    "xai",
    "xiaomi",
}
# Moving aliases the source lists only under another provider's row.
KEEP_UNLISTED = {
    ("google-vertex", "gemini-flash-latest"),
    ("google-vertex", "gemini-flash-lite-latest"),
}
# The providers whose listing is where a model lives, written with the catalog
# as `modelVendors` — a provider and the id prefixes of the models that are
# its own: the makers (the own-catalog providers minus the inference hosts,
# whose listings are all their own), plus the makers' plans. A plan also
# serves other makers' models — the Qwen token plans carry GLM, Kimi and
# DeepSeek — so it is where a model lives only under its own family's names.
# A model adopted from another provider's listing takes the first of these
# that carries its id; a host's copy of it is not where it lives.
HOSTS = {
    "baseten",
    "cerebras",
    "cloudflare-workers-ai",
    "fireworks",
    "groq",
    "huggingface",
    "nvidia",
    "together",
}
MAKER_PLANS = {
    "kimi-coding": ("kimi",),
    "qwen-token-plan": ("qwen", "qvq", "qwq"),
    "qwen-token-plan-cn": ("qwen", "qvq", "qwq"),
    "qwen-token-plan-individual": ("qwen", "qvq", "qwq"),
    "zai": ("glm",),
    "zai-coding-cn": ("glm",),
}


def model_vendors() -> dict[str, list[str]]:
    """`modelVendors` as the catalog carries it: provider id → the lowercase
    id prefixes of the models that are its own, empty when all of them are."""
    vendors = {provider: [] for provider in RETIRE_UNLISTED - HOSTS}
    vendors.update({provider: sorted(prefixes) for provider, prefixes in MAKER_PLANS.items()})
    return dict(sorted(vendors.items()))


def normalize_url(url: object) -> str:
    return (url or "").strip().rstrip("/").lower() if isinstance(url, str) else ""


def load_source(path: str | None) -> dict:
    if path:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    # The CDN rejects urllib's default User-Agent with a 403.
    request = urllib.request.Request(
        SOURCE_URL,
        headers={"User-Agent": "holt-update-model-catalog", "Accept": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        return json.load(response)


def match_providers(catalog: dict, source: dict, notes: list[str]) -> dict[str, str]:
    by_url: dict[str, list[str]] = defaultdict(list)
    for source_id, provider in source.items():
        if provider.get("api"):
            by_url[normalize_url(provider["api"])].append(source_id)
    matches: dict[str, str] = {}
    for provider_id, models in catalog.items():
        if provider_id in SOURCE_OVERRIDE:
            matches[provider_id] = SOURCE_OVERRIDE[provider_id]
            continue
        urls = {normalize_url(model.get("baseUrl")) for model in models.values()}
        hits = sorted({sid for url in urls if url for sid in by_url.get(url, [])})
        if len(hits) > 1:
            chosen = provider_id if provider_id in hits else hits[0]
            notes.append(f"{provider_id}: baseUrl matches {hits}; using {chosen}")
            hits = [chosen]
        if hits:
            matches[provider_id] = hits[0]
        elif provider_id in source:
            matches[provider_id] = provider_id
        else:
            notes.append(f"{provider_id}: no source provider, left unchanged")
    return matches


def effort_values(options: object) -> list[str] | None:
    """The `effort` option's values, or None when the source names none."""
    for option in options or []:
        if isinstance(option, dict) and option.get("type") == "effort" and option.get("values"):
            return list(option["values"])
    return None


def ladder_from_source(model: dict) -> dict | None:
    """`thinkingLevelMap` for a source model that names effort levels, else None.

    Every level is spelled out (the absent ones as null) so the map says what
    it means regardless of the compact forms the pi generator emitted.
    `off` is `"none"` when the vendor names that value, absent when a toggle
    disables thinking through the format's own switch, null when thinking
    cannot be turned off.
    """
    options = model.get("reasoning_options") or []
    values = effort_values(options)
    if values is None:
        return None
    toggle = any(isinstance(o, dict) and o.get("type") == "toggle" for o in options)
    level_map: dict = {}
    if "none" in values:
        level_map["off"] = "none"
    elif not toggle:
        level_map["off"] = None
    for level in LEVELS:
        level_map[level] = level if level in values else None
    return level_map


def merged_ladder(existing: dict | None, source_map: dict) -> dict:
    """The source's ladder for an existing entry, keeping what the catalog knows
    better: a level the catalog maps to a value other than its own name (`minimal`
    → `low` on Copilot Claude, `off` → `default` on Groq Qwen) is a transport
    decision, not a vendor fact, and so is the `off` entry — pi's "cannot turn
    thinking off" on Claude and Kimi K3 is not something the source can take
    back. Plain `"low": "low"` entries follow the source."""
    merged = dict(source_map)
    existing = existing or {}
    if "off" in existing:
        merged["off"] = existing["off"]
    for level in LEVELS:
        value = existing.get(level)
        if isinstance(value, str) and value != level:
            merged[level] = value
    return merged


def dated_snapshot(model_id: str) -> bool:
    """A date-stamped id (`-20250929`, `-250729`, `-2024-05-13`) — the engine's
    picker hides these behind the moving alias, so the catalog does not grow
    them."""
    parts = re.split(r"[-_.]", model_id)
    if any(len(p) in (6, 8) and p.isdigit() for p in parts):
        return True
    return bool(re.search(r"\d{4}-\d{2}-\d{2}", model_id))


def ladder_key(level_map: dict | None) -> tuple:
    """What the engine offers for a map: (supported levels, thinking can be off)."""
    level_map = level_map or {}
    supported = frozenset(
        level
        for level in LEVELS
        if level_map.get(level) or (level in DEFAULT_ON and level not in level_map)
    )
    off = not (level_map.get("off", "absent") is None)
    return (supported, off)


def is_chat_model(model: dict) -> bool:
    modalities = model.get("modalities") or {}
    return (
        model.get("type") not in NON_CHAT_TYPES
        and "text" in (modalities.get("input") or [])
        and "text" in (modalities.get("output") or [])
    )


def bare_id(model_id: str) -> str:
    """The id without its vendor prefix (`openai/gpt-5` → `gpt-5`)."""
    return model_id.rsplit("/", 1)[-1]


def realtime_ids(source: dict) -> set[str]:
    """The bare ids the source types as realtime, under any provider."""
    return {
        bare_id(model_id)
        for provider in source.values()
        for model_id, model in (provider.get("models") or {}).items()
        if model.get("type") in REMOVE_EXISTING_TYPES
    }


def removal_reason(
    provider_id: str, model_id: str, model: dict | None, realtime_anywhere: set[str]
) -> str | None:
    """Why an existing entry leaves the catalog, or None when it stays.

    The provider's own row decides when it has one: realtime or deprecated.
    A model the provider no longer lists goes when any provider marks that id
    realtime, and where the source's listing is the provider's own catalog —
    a moving alias in KEEP_UNLISTED stays.
    """
    if model is not None:
        if model.get("type") in REMOVE_EXISTING_TYPES:
            return "realtime (removed)"
        if model.get("status") == "deprecated":
            return "deprecated (removed)"
        return None
    if bare_id(model_id) in realtime_anywhere:
        return "realtime (removed)"
    if provider_id in RETIRE_UNLISTED and (provider_id, model_id) not in KEEP_UNLISTED:
        return "unlisted (removed)"
    return None


def input_from_source(model: dict) -> list[str] | None:
    modalities = model.get("modalities") or {}
    if "input" not in modalities:
        return None
    inputs = ["text"]
    if "image" in (modalities["input"] or []):
        inputs.append("image")
    return inputs


def cost_from_source(model: dict) -> dict | None:
    cost = model.get("cost")
    if not isinstance(cost, dict):
        return None
    return {
        "input": cost.get("input", 0),
        "output": cost.get("output", 0),
        "cacheRead": cost.get("cache_read", 0),
        "cacheWrite": cost.get("cache_write", 0),
    }


def is_zero_cost(cost: dict) -> bool:
    """Whether every source price is zero."""
    return not any(cost.values())


def is_explicitly_free(model_id: str) -> bool:
    """A free variant, not a subscription or unknown price reported as zero."""
    return model_id.endswith((":free", "-free"))


def set_after(entry: dict, anchor: str, key: str, value: object) -> None:
    """Set `key` on `entry`, placed right after `anchor` when it is new."""
    if key in entry:
        entry[key] = value
        return
    rebuilt = {}
    for existing_key, existing_value in entry.items():
        rebuilt[existing_key] = existing_value
        if existing_key == anchor:
            rebuilt[key] = value
    entry.clear()
    entry.update(rebuilt)


def refresh_entry(model_id: str, entry: dict, model: dict, changes: Counter) -> None:
    """Overwrite the source-owned facts on an existing catalog entry, in place."""
    name = model.get("name")
    if name and name != entry.get("name"):
        entry["name"] = name
        changes["name"] += 1
    reasoning = bool(model.get("reasoning"))
    if reasoning != bool(entry.get("reasoning")):
        entry["reasoning"] = reasoning
        changes["reasoning"] += 1
    level_map = ladder_from_source(model)
    if level_map is not None:
        existing_map = entry.get("thinkingLevelMap")
        level_map = merged_ladder(existing_map, level_map)
        if level_map != existing_map:
            if ladder_key(level_map) != ladder_key(existing_map):
                changes["thinkingLevelMap"] += 1
            else:
                changes["thinkingLevelMap (spelled out)"] += 1
            set_after(entry, "reasoning", "thinkingLevelMap", level_map)
    inputs = input_from_source(model)
    if inputs and inputs != entry.get("input"):
        entry["input"] = inputs
        changes["input"] += 1
    cost = cost_from_source(model)
    if cost and (not is_zero_cost(cost) or is_explicitly_free(model_id)):
        if is_zero_cost(cost):
            # An explicitly free variant has no paid tiers. Other all-zero
            # prices mean subscription or unknown and keep the reference cost.
            merged = cost
        else:
            # A tiered price (`tiers`) is catalog knowledge the source does not
            # carry and stays.
            merged = {**entry.get("cost", {}), **cost}
        if merged != entry.get("cost"):
            entry["cost"] = merged
            changes["cost"] += 1
    limit = model.get("limit") or {}
    if limit.get("context") and limit["context"] != entry.get("contextWindow"):
        entry["contextWindow"] = limit["context"]
        changes["contextWindow"] += 1
    if limit.get("output") and limit["output"] != entry.get("maxTokens"):
        entry["maxTokens"] = limit["output"]
        changes["maxTokens"] += 1


def transport_of(entry: dict) -> list:
    return [entry.get("api"), entry.get("baseUrl"), entry.get("compat"), entry.get("headers")]


def id_prefix(model_id: str) -> tuple[str, str]:
    """The id's vendor or family head and its OpenRouter-style variant —
    (`anthropic`, "") for `anthropic/claude-opus-5`, (`anthropic`, "batch") for
    `anthropic/claude-opus-5:batch`, (`gpt`, "") for `gpt-5.4-mini`: the
    grouping gateways route by (a `:batch` Claude rides the OpenAI endpoint,
    the plain one the Anthropic endpoint)."""
    head, _, variant = model_id.partition(":")
    if "/" in head:
        return (head.split("/")[0], variant)
    return (head.split("-")[0], variant)


def base_family(model_id: str) -> tuple[str, str] | None:
    """The family a version bump in the id's head sits in (`k3.1-pro` → the
    `k3` family), or None when the head carries no such bump."""
    head, _, variant = model_id.partition(":")
    if "/" in head:
        return None
    family, dot, version = head.split("-")[0].rpartition(".")
    if dot and version.isdigit():
        return (family, variant)
    return None


def build_entry(provider_id: str, model_id: str, model: dict, models: dict) -> dict | None:
    """A catalog entry for a source model the catalog lacks, or None when the
    provider's transport cannot be chosen for it.

    models.dev carries no transport shape, so api/baseUrl/compat/headers (and
    the input list, when the source omits modalities) come from the existing
    model the new one most resembles: the same id prefix — the grouping the
    gateways route by — narrowed to the same ladder when that subset exists,
    with the most common shape winning. A version bump in that prefix
    (`k3.1-pro`) rides with the family it bumped (`k3`). A prefix the catalog
    splits across transports, or one it has never seen (outside
    INHERIT_DOMINANT), is pi's per-model knowledge the source does not carry:
    the model is skipped and noted for a hand entry rather than shipped with a
    guessed transport."""
    level_map = ladder_from_source(model)
    group = [e for e in models.values() if id_prefix(e["id"]) == id_prefix(model_id)]
    if not group:
        family = base_family(model_id)
        if family is not None:
            group = [e for e in models.values() if id_prefix(e["id"]) == family]
    if len({(e.get("api"), e.get("baseUrl")) for e in group}) > 1:
        return None
    if not group:
        if provider_id not in INHERIT_DOMINANT:
            return None
        dominant = Counter((e.get("api"), e.get("baseUrl")) for e in models.values()).most_common(1)[0][0]
        group = [e for e in models.values() if (e.get("api"), e.get("baseUrl")) == dominant]
    same_ladder = [e for e in group if ladder_key(e.get("thinkingLevelMap")) == ladder_key(level_map)]
    pool = same_ladder or group
    by_shape: dict[str, list[dict]] = defaultdict(list)
    for candidate in pool:
        by_shape[json.dumps(transport_of(candidate), sort_keys=True)].append(candidate)
    sibling = max(by_shape.values(), key=len)[0]

    entry = {"id": model_id, "name": model.get("name") or model_id, "api": sibling.get("api"), "provider": provider_id}
    if sibling.get("headers"):
        entry["headers"] = sibling["headers"]
    entry.update({"baseUrl": sibling.get("baseUrl"), "reasoning": bool(model.get("reasoning"))})
    if level_map is not None:
        entry["thinkingLevelMap"] = level_map
    entry["input"] = input_from_source(model) or list(sibling.get("input", ["text"]))
    cost = cost_from_source(model)
    if not cost or (is_zero_cost(cost) and not is_explicitly_free(model_id)):
        cost = sibling.get("cost") or {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
    entry["cost"] = cost
    if sibling.get("compat"):
        entry["compat"] = sibling["compat"]
    limit = model["limit"]
    entry["contextWindow"] = limit["context"]
    entry["maxTokens"] = limit["output"]
    return entry


def integral_floats(value):
    """Whole floats as ints — the catalog writes `1`, never `1.0`."""
    if isinstance(value, float) and value.is_integer():
        return int(value)
    if isinstance(value, dict):
        return {k: integral_floats(v) for k, v in value.items()}
    if isinstance(value, list):
        return [integral_floats(v) for v in value]
    return value


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--source", help="a downloaded models.dev api.json instead of a fetch")
    parser.add_argument("--catalog", type=Path, default=CATALOG, help="the catalog to rewrite in place")
    parser.add_argument("--dry-run", action="store_true", help="report what would change, write nothing")
    args = parser.parse_args(argv)

    document = json.loads(args.catalog.read_text(encoding="utf-8"))
    catalog = document["models"]
    before = sum(len(models) for models in catalog.values())
    source = load_source(args.source)
    realtime_anywhere = realtime_ids(source)

    notes: list[str] = []
    matches = match_providers(catalog, source, notes)
    refresh_only = set(REFRESH_ONLY)
    fed: dict[str, list[str]] = defaultdict(list)
    for provider_id, source_id in matches.items():
        fed[source_id].append(provider_id)
    for source_id, provider_ids in fed.items():
        if len(provider_ids) > 1:
            refresh_only.update(provider_ids)
            notes.append(f"{source_id} feeds {sorted(provider_ids)}: refreshing existing models only, adding none")

    totals: Counter = Counter()
    for provider_id in sorted(catalog):
        source_id = matches.get(provider_id)
        if not source_id:
            continue
        provider_models = catalog[provider_id]
        source_models = source[source_id].get("models", {})
        changes: Counter = Counter()
        for model_id, entry in list(provider_models.items()):
            model = source_models.get(model_id)
            reason = removal_reason(provider_id, model_id, model, realtime_anywhere)
            if reason is not None:
                del provider_models[model_id]
                changes[reason] += 1
                continue
            if model is None:
                changes["not in source (kept)"] += 1
            else:
                refresh_entry(model_id, entry, model, changes)
        added = []
        skipped: list[str] = []
        if provider_id not in refresh_only:
            for model_id, model in source_models.items():
                if model_id in provider_models or model.get("status") == "deprecated":
                    continue
                if (provider_id, model_id) in SKIP_ADDITIONS:
                    changes["excluded by hand (skipped)"] += 1
                    continue
                if dated_snapshot(model_id):
                    changes["dated snapshot (skipped)"] += 1
                    continue
                if not is_chat_model(model):
                    continue
                limit = model.get("limit") or {}
                if not (limit.get("context") and limit.get("output")):
                    notes.append(f"{provider_id}/{model_id}: source has no context/output limit, not added")
                    continue
                entry = build_entry(provider_id, model_id, model, provider_models)
                if entry is None:
                    changes["no transport sibling (skipped)"] += 1
                    skipped.append(model_id)
                    continue
                provider_models[model_id] = entry
                added.append(model_id)
        if skipped:
            notes.append(f"{provider_id}: transport not derivable from a same-prefix model, not added: {', '.join(skipped)}")
        totals.update(changes)
        totals["added"] += len(added)
        fields = ", ".join(f"{key} {count}" for key, count in sorted(changes.items()))
        print(f"{provider_id} <- {source_id}: {len(provider_models)} models, added {len(added)}" + (f" ({fields})" if fields else ""))
    after = sum(len(models) for models in catalog.values())

    print()
    print(f"models: {before} -> {after}; changed fields: {dict(totals)}")
    for note in notes:
        print(f"note: {note}")
    if args.dry_run:
        print("dry run: nothing written")
        return 0
    now = datetime.now(timezone.utc)
    document["generatedAt"] = now.strftime("%Y-%m-%dT%H:%M:%S.") + f"{now.microsecond // 1000:03d}Z"
    document["modelVendors"] = model_vendors()
    text = json.dumps(integral_floats(document), indent="\t", ensure_ascii=False) + "\n"
    args.catalog.write_text(text, encoding="utf-8")
    print(f"wrote {args.catalog}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
