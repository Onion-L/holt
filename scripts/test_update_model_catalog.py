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

    def test_deprecated_source_models_are_removed(self) -> None:
        sibling = catalog_entry("vendor/current")
        old = catalog_entry("vendor/old")
        refreshed = self.run_refresh(
            {entry["id"]: entry for entry in [sibling, old]},
            {
                "vendor/current": source_model("vendor/current"),
                "vendor/old": {**source_model("vendor/old"), "status": "deprecated"},
            },
        )

        self.assertNotIn("vendor/old", refreshed)
        self.assertIn("vendor/current", refreshed)

    def test_unlisted_models_leave_where_the_source_lists_the_provider(self) -> None:
        current = catalog_entry("k3")
        retired = catalog_entry("k1")
        refreshed = self.run_refresh(
            {entry["id"]: entry for entry in [current, retired]},
            {"k3": source_model("k3")},
            provider_id="moonshotai",
        )

        self.assertNotIn("k1", refreshed)
        self.assertIn("k3", refreshed)

    def test_a_moving_alias_stays_where_the_source_lists_it_elsewhere(self) -> None:
        current = catalog_entry("gemini-4-neon")
        alias = catalog_entry("gemini-flash-latest")
        refreshed = self.run_refresh(
            {entry["id"]: entry for entry in [current, alias]},
            {"gemini-4-neon": source_model("gemini-4-neon")},
            provider_id="google-vertex",
        )

        self.assertIn("gemini-flash-latest", refreshed)

    def test_plan_providers_keep_their_unlisted_models(self) -> None:
        # A plan's models.dev listing lags what the plan still serves; only an
        # explicit deprecated or realtime row retires a plan model.
        current = catalog_entry("glm-5.4")
        unlisted = catalog_entry("glm-5.2")
        refreshed = self.run_refresh(
            {entry["id"]: entry for entry in [current, unlisted]},
            {"glm-5.4": source_model("glm-5.4")},
            provider_id="zai-coding-cn",
        )

        self.assertIn("glm-5.2", refreshed)


class VendorListTests(unittest.TestCase):
    def test_the_generator_writes_the_vendor_list(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            catalog_path = Path(directory) / "catalog.json"
            source_path = Path(directory) / "source.json"
            document = {"models": {}, "imageModels": {}, "generatedAt": "2026-01-01T00:00:00.000Z"}
            catalog_path.write_text(json.dumps(document), encoding="utf-8")
            source_path.write_text(json.dumps({}), encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()):
                result = update_model_catalog.main(
                    ["--source", str(source_path), "--catalog", str(catalog_path)]
                )
            self.assertEqual(result, 0)
            document = json.loads(catalog_path.read_text(encoding="utf-8"))

        self.assertEqual(document["modelVendors"], update_model_catalog.model_vendors())

    def test_the_committed_catalog_carries_the_vendors(self) -> None:
        # The engine adopts a listing's id from the first vendor that carries
        # it, reading the list off the catalog: the committed file must hold
        # exactly what the generator would write, over providers it carries.
        document = json.loads(update_model_catalog.CATALOG.read_text(encoding="utf-8"))
        vendors = update_model_catalog.model_vendors()

        self.assertEqual(document["modelVendors"], vendors)
        for vendor, prefixes in vendors.items():
            self.assertIn(vendor, document["models"])
            for prefix in prefixes:
                self.assertEqual(prefix, prefix.lower())

    def test_a_plan_is_a_vendor_only_for_its_own_family(self) -> None:
        # The Qwen token plans serve GLM, Kimi and DeepSeek beside Qwen: a
        # GLM id must not take its facts from their copy of it.
        vendors = update_model_catalog.model_vendors()

        self.assertEqual(vendors["qwen-token-plan"], ["qvq", "qwen", "qwq"])
        self.assertEqual(vendors["zai-coding-cn"], ["glm"])
        self.assertEqual(vendors["kimi-coding"], ["kimi"])
        self.assertEqual(vendors["openai"], [])
        for host in update_model_catalog.HOSTS:
            self.assertNotIn(host, vendors)


if __name__ == "__main__":
    unittest.main()
