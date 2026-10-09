import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "docker_release_metadata", Path(__file__).resolve().parents[1] / "scripts/docker-release-metadata.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class DockerReleaseMetadataTests(unittest.TestCase):
    def derive(self, tag, version=None, user="dockeruser", prerelease=False):
        return module.metadata(tag, version or tag[1:], "P4UL-M/deep-obsidian-mcp", user, prerelease)

    def test_stable_release_names_both_registries(self):
        result = self.derive("v0.2.0")
        self.assertEqual(result["channel"], "latest")
        self.assertEqual(result["ghcr"], "ghcr.io/p4ul-m/deep-obsidian-mcp")
        self.assertEqual(result["dockerhub"], "docker.io/dockeruser/deep-obsidian-mcp")
        self.assertEqual(result["tag"], "v0.2.0")

    def test_prereleases_never_replace_latest(self):
        for suffix, channel in [("alpha.6", "alpha"), ("beta.1", "beta"),
                                ("rc.1", "rc"), ("experimental.1", "preview")]:
            with self.subTest(suffix=suffix):
                self.assertEqual(self.derive(f"v0.2.0-{suffix}")["channel"], channel)
        self.assertEqual(self.derive("v0.2.0", prerelease=True)["channel"], "preview")

    def test_version_mismatch_and_unsafe_tags_are_rejected(self):
        for tag, version in [("v0.2.1", "0.2.0"), ("main", "0.2.0"),
                             ("v0.2.0+build", "0.2.0+build"), ("v0.2.0\nlatest", "0.2.0")]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                self.derive(tag, version=version)

    def test_missing_or_unsafe_dockerhub_username_is_rejected(self):
        for user in ["", "Name", "user/repo", "user\nother", "$(command)"]:
            with self.subTest(user=user), self.assertRaises(ValueError):
                self.derive("v0.2.0", user=user)


if __name__ == "__main__":
    unittest.main()
