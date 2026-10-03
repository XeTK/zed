#!/usr/bin/env python3
"""Checks ci/update-server/features.json against the code it describes.

    python3 ci/update-server/test_features.py
"""
import json
import os
import re
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))


def load_features():
    with open(os.path.join(HERE, "features.json"), encoding="utf-8") as file:
        return json.load(file)


def default_settings_text():
    with open(os.path.join(ROOT, "assets", "settings", "default.json"), encoding="utf-8") as file:
        return file.read()


class FeaturesTest(unittest.TestCase):
    def setUp(self):
        self.data = load_features()

    def test_sections_are_unique_and_titled(self):
        ids = [section["id"] for section in self.data["sections"]]
        self.assertEqual(len(ids), len(set(ids)), "duplicate section id")
        for section in self.data["sections"]:
            self.assertTrue(section["title"].strip(), section)

    def test_every_feature_is_complete(self):
        section_ids = {section["id"] for section in self.data["sections"]}
        allowed = {"section", "title", "summary", "details", "example", "settings", "pr"}
        for feature in self.data["features"]:
            title = feature.get("title", "<untitled>")
            self.assertLessEqual(set(feature), allowed, f"{title}: unknown field")
            self.assertIn(feature.get("section"), section_ids, f"{title}: unknown section")
            self.assertTrue(feature.get("title", "").strip(), "a feature has no title")
            details = feature.get("details")
            self.assertTrue(
                isinstance(details, list) and details and all(isinstance(line, str) and line.strip() for line in details),
                f"{title}: details must be a non-empty list of strings",
            )
            if "pr" in feature:
                self.assertIsInstance(feature["pr"], int, f"{title}: pr must be a number")

    def test_titles_are_unique(self):
        titles = [feature["title"] for feature in self.data["features"]]
        self.assertEqual(len(titles), len(set(titles)), "duplicate feature title")

    def test_every_section_is_used(self):
        used = {feature["section"] for feature in self.data["features"]}
        for section in self.data["sections"]:
            self.assertIn(section["id"], used, f"section {section['id']} has no features")

    def test_settings_named_by_features_exist(self):
        defaults = default_settings_text()
        for feature in self.data["features"]:
            for key in feature.get("settings", []):
                self.assertTrue(
                    re.search(r'"' + re.escape(key) + r'"\s*:', defaults),
                    f"{feature['title']}: setting {key!r} is not in assets/settings/default.json",
                )

    def test_examples_are_valid_json_and_use_listed_settings(self):
        for feature in self.data["features"]:
            example = feature.get("example")
            if not example:
                continue
            parsed = json.loads(example)

            def keys(value):
                if isinstance(value, dict):
                    for key, inner in value.items():
                        yield key
                        yield from keys(inner)

            for key in keys(parsed):
                if key == "agent":
                    continue
                self.assertIn(key, feature.get("settings", []), f"{feature['title']}: example key {key!r} is not listed in settings")


if __name__ == "__main__":
    unittest.main()
