#!/usr/bin/env python3
"""Regression tests for update-model-catalog.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True

SCRIPT = Path(__file__).with_name("update-model-catalog.py")
SPEC = importlib.util.spec_from_file_location("update_model_catalog", SCRIPT)
update_model_catalog = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(update_model_catalog)

BASE_URL = "https://openrouter.ai/api/v1"
PAID_COST = {"input": 1, "output": 2, "cache_read": 0.1, "cache_write": 0}
FREE_COST = {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0}


def catalog_entry(model_id: str, cost: dict | None = None) -> dict:
    source_cost = cost or PAID_COST
    return {
        "id": model_id,
        "name": model_id,
        "api": "openai-completions",
        "provider": "openrouter",
        "baseUrl": BASE_URL,
        "reasoning": False,
        "input": ["text"],
        "cost": {
            "input": source_cost["input"],
            "output": source_cost["output"],
            "cacheRead": source_cost["cache_read"],
            "cacheWrite": source_cost["cache_write"],
        },
        "contextWindow": 1000,
        "maxTokens": 100,
    }


def source_model(model_id: str, cost: dict | None = None, model_type: str | None = None) -> dict:
    model = {
        "id": model_id,
        "name": model_id,
        "reasoning": False,
        "modalities": {"input": ["text"], "output": ["text"]},
        "limit": {"context": 1000, "output": 100},
        "cost": cost or PAID_COST,
    }
    if model_type is not None:
        model["type"] = model_type
    return model


class CatalogRefreshTests(unittest.TestCase):
    def run_refresh(
        self,
        catalog_models: dict,
        source_models: dict,
        other_providers: dict | None = None,
        provider_id: str = "openrouter",
    ) -> dict:
        document = {
            "models": {provider_id: catalog_models},
            "imageModels": {},
            "generatedAt": "2026-01-01T00:00:00.000Z",
        }
        source = {provider_id: {"api": BASE_URL, "models": source_models}}
        source.update(other_providers or {})
        with tempfile.TemporaryDirectory() as directory:
            catalog_path = Path(directory) / "catalog.json"
            source_path = Path(directory) / "source.json"
            catalog_path.write_text(json.dumps(document), encoding="utf-8")
            source_path.write_text(json.dumps(source), encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()):
                result = update_model_catalog.main(
                    ["--source", str(source_path), "--catalog", str(catalog_path)]
                )
            self.assertEqual(result, 0)
            document = json.loads(catalog_path.read_text(encoding="utf-8"))
            return document["models"][provider_id]

    def test_realtime_entries_are_not_added_and_existing_ones_are_removed(self) -> None:
        sibling = catalog_entry("openai/gpt-5")
        realtime = catalog_entry("openai/gpt-realtime-1.5")
        legacy = catalog_entry("legacy/model")
        refreshed = self.run_refresh(
            {entry["id"]: entry for entry in [sibling, realtime, legacy]},
            {
                "openai/gpt-5": source_model("openai/gpt-5"),
                "openai/gpt-realtime-1.5": source_model(
                    "openai/gpt-realtime-1.5", model_type="realtime"
                ),
                "openai/gpt-realtime-2": source_model(
                    "openai/gpt-realtime-2", model_type="realtime"
                ),
            },
        )

        self.assertNotIn("openai/gpt-realtime-1.5", refreshed)
        self.assertNotIn("openai/gpt-realtime-2", refreshed)
        self.assertIn("legacy/model", refreshed)

    def test_an_unlisted_entry_is_removed_when_marked_realtime_elsewhere(self) -> None:
        sibling = catalog_entry("openai/gpt-5")
        unlisted = catalog_entry("gpt-realtime-2.1")
        listed_as_chat = catalog_entry("gpt-realtime-1.5")
        refreshed = self.run_refresh(
            {entry["id"]: entry for entry in [sibling, unlisted, listed_as_chat]},
            {
                "openai/gpt-5": source_model("openai/gpt-5"),
                "gpt-realtime-1.5": source_model("gpt-realtime-1.5"),
            },
            {
                "gateway": {
                    "api": "https://gateway.example/v1",
                    "models": {
                        "openai/gpt-realtime-1.5": source_model(
                            "openai/gpt-realtime-1.5", model_type="realtime"
                        ),
                        "openai/gpt-realtime-2.1": source_model(
                            "openai/gpt-realtime-2.1", model_type="realtime"
                        ),
                    },
                }
            },
        )

        self.assertNotIn("gpt-realtime-2.1", refreshed)
        # The provider's own row decides once it has one.
        self.assertIn("gpt-realtime-1.5", refreshed)

    def test_explicitly_free_models_keep_zero_prices(self) -> None:
        sibling = catalog_entry("openai/gpt-5")
        existing_free = catalog_entry("newvendor/existing:free")
        subscription = catalog_entry("subscription/existing")
        existing_free["cost"]["tiers"] = [
            {
                "inputTokensAbove": 500,
                "input": 3,
                "output": 4,
                "cacheRead": 0.3,
                "cacheWrite": 0,
            }
        ]
        refreshed = self.run_refresh(
            {entry["id"]: entry for entry in [sibling, existing_free, subscription]},
            {
                "openai/gpt-5": source_model("openai/gpt-5"),
                "newvendor/existing:free": source_model(
                    "newvendor/existing:free", FREE_COST
                ),
                "newvendor/new:free": source_model("newvendor/new:free", FREE_COST),
                "newvendor/dash-free": source_model("newvendor/dash-free", FREE_COST),
                "othervendor/unknown": source_model("othervendor/unknown", FREE_COST),
                "subscription/existing": source_model(
                    "subscription/existing", FREE_COST
                ),
            },
        )

        for model_id in [
            "newvendor/existing:free",
            "newvendor/new:free",
            "newvendor/dash-free",
        ]:
            cost = refreshed[model_id]["cost"]
            self.assertEqual(
                cost,
                {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                model_id,
            )
        # A subscription or unknown zero is no measured price: a new model
        # takes a sibling's reference price, an existing one keeps its own.
        self.assertEqual(refreshed["othervendor/unknown"]["cost"], sibling["cost"])
        self.assertEqual(refreshed["subscription/existing"]["cost"], subscription["cost"])

    def test_a_version_bump_inherits_the_base_family_transport(self) -> None:
        k3 = catalog_entry("k3")
        k3["compat"] = {"forceAdaptiveThinking": True}
        refreshed = self.run_refresh(
            {entry["id"]: entry for entry in [k3, catalog_entry("k3-256k")]},
            {
                "k3": source_model("k3"),
                "k3-256k": source_model("k3-256k"),
                "k3.1-pro": source_model("k3.1-pro"),
                "k4-pro": source_model("k4-pro"),
            },
            provider_id="plan",
        )

        # `k3.1-pro` bumped the `k3` family; `k4-pro` starts one the catalog has
        # never seen, so its transport is still not derivable.
        self.assertEqual(refreshed["k3.1-pro"]["compat"], k3["compat"])
        self.assertNotIn("k4-pro", refreshed)


if __name__ == "__main__":
    unittest.main()
