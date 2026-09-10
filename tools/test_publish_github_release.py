from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest import mock


TOOLS = Path(__file__).resolve().parent
SCRIPT = TOOLS / "publish-github-release.py"
SPEC = importlib.util.spec_from_file_location("publish_github_release", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
publisher = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = publisher
SPEC.loader.exec_module(publisher)


COMMIT = "a" * 40


class FixtureApi:
    def __init__(self, assets, *, tag="v0.189.17", stable=None):
        self.assets = tuple(assets)
        self.tag = tag
        self.immutable = True
        self.rulesets = [
            publisher.TagRuleset(
                target="tag",
                enforcement="active",
                includes=("v*.*.*",),
                excludes=(),
                restrictions=frozenset({"update", "deletion"}),
            )
        ]
        self.tag_commit = COMMIT
        self.tag_annotated = True
        self.releases: dict[str, publisher.Release] = {}
        self.next_id = 1
        self.events: list[tuple[str, object]] = []
        self.stable = list(stable or [])
        self.publish_timeout = False
        self.publish_timeout_before_commit = False
        self.publish_server_error = False
        self.upload_timeout_for: str | None = None
        self.immutable_checks = 0

    def immutable_releases_enabled(self):
        self.events.append(("immutable", None))
        self.immutable_checks += 1
        return self.immutable

    def tag_rulesets(self):
        self.events.append(("rulesets", None))
        return tuple(self.rulesets)

    def tag_ref(self, tag):
        self.events.append(("tag", tag))
        return publisher.TagRef(self.tag_commit, annotated=self.tag_annotated)

    def get_release(self, tag):
        self.events.append(("get", tag))
        return self.releases.get(tag)

    def list_releases(self):
        self.events.append(("list", None))
        return tuple(self.stable) + tuple(self.releases.values())

    def create_draft(self, tag, title):
        self.events.append(("create", (tag, title)))
        release = publisher.Release(
            id=self.next_id,
            tag_name=tag,
            draft=True,
            prerelease=False,
            immutable=False,
            assets=(),
            upload_url="fixture://upload",
        )
        self.next_id += 1
        self.releases[tag] = release
        return release

    def upload_asset(self, release, asset):
        self.events.append(("upload", asset.name))
        if self.upload_timeout_for == asset.name:
            raise publisher.ApiTimeout(f"fixture timeout for {asset.name}")
        digest = asset.verify_local()
        uploaded = publisher.ReleaseAsset(
            name=asset.name,
            size=len(digest),
            sha256=hashlib.sha256(digest).hexdigest(),
        )
        current = self.releases[release.tag_name]
        updated = replace(current, assets=current.assets + (uploaded,))
        self.releases[release.tag_name] = updated
        return uploaded

    def publish_release(self, release, *, make_latest):
        self.events.append(("publish", make_latest))
        if not self.publish_timeout_before_commit:
            current = self.releases[release.tag_name]
            if not self.publish_timeout:
                published = replace(current, draft=False, immutable=True)
            else:
                published = replace(current, draft=False, immutable=True)
            self.releases[release.tag_name] = published
        if self.publish_timeout or self.publish_timeout_before_commit:
            raise publisher.ApiTimeout("fixture publish timeout")
        if self.publish_server_error:
            raise publisher.ApiError("fixture 502 after commit", status=502)
        return self.releases[release.tag_name]


def make_assets(root: Path, *, names=("forge-a.tar.gz", "forge-b.whl")):
    release_dir = root / "release"
    release_dir.mkdir()
    entries = []
    for index, name in enumerate(names):
        data = f"fixture-{index}\n".encode("ascii")
        path = release_dir / name
        path.write_bytes(data)
        entries.append(
            {
                "name": name,
                "size": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            }
        )
    manifest = root / "manifest.json"
    manifest.write_text(
        json.dumps({"assets": entries}, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return release_dir, manifest


def load_assets(root: Path):
    release_dir, manifest = make_assets(root)
    return release_dir, manifest, publisher.load_manifest(release_dir, manifest)


def make_publisher(root: Path, api: FixtureApi, *, tag=None):
    release_dir = root / "release"
    manifest = root / "manifest.json"
    if release_dir.is_dir() and manifest.is_file():
        assets = publisher.load_manifest(release_dir, manifest)
    else:
        release_dir, manifest, assets = load_assets(root)
    tag = tag or api.tag
    return publisher.ReleasePublisher(
        api,
        tag=tag,
        expected_commit=COMMIT,
        title=f"Forge {tag}",
        assets=assets,
    )


def release_with_assets(api: FixtureApi, *, draft: bool, immutable: bool, extra=()):
    remote = tuple(
        publisher.ReleaseAsset(
            name=asset.name,
            size=asset.size,
            sha256=asset.sha256,
        )
        for asset in api.assets
    )
    remote += tuple(extra)
    return publisher.Release(
        id=77,
        tag_name=api.tag,
        draft=draft,
        prerelease=False,
        immutable=immutable,
        assets=remote,
        upload_url="fixture://upload",
    )


class ManifestTests(unittest.TestCase):
    def test_manifest_is_an_exact_allowlist_and_rejects_unlisted_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release_dir, manifest, _ = load_assets(root)
            (release_dir / "unlisted.txt").write_text("no", encoding="ascii")
            with self.assertRaisesRegex(publisher.ManifestError, "unlisted"):
                publisher.load_manifest(release_dir, manifest)

    def test_sha256sums_format_is_supported_and_size_is_derived(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release_dir, manifest, _ = load_assets(root)
            records = json.loads(manifest.read_text(encoding="utf-8"))["assets"]
            manifest.write_text(
                "".join(f"{item['sha256']}  {item['name']}\n" for item in records),
                encoding="ascii",
            )
            assets = publisher.load_manifest(release_dir, manifest)
            self.assertEqual([asset.name for asset in assets], ["forge-a.tar.gz", "forge-b.whl"])

    def test_manifest_rejects_symlink_asset(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release_dir, manifest, _ = load_assets(root)
            target = release_dir / "forge-a.tar.gz"
            replacement = release_dir / "real.bin"
            target.rename(replacement)
            target.symlink_to(replacement.name)
            with self.assertRaisesRegex(publisher.ManifestError, "regular file"):
                publisher.load_manifest(release_dir, manifest)

    def test_json_manifest_controls_are_explicit_release_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release_dir, _, assets = load_assets(root)
            checksums = release_dir / "SHA256SUMS"
            checksums.write_text(
                "\n".join(f"{asset.sha256}  {asset.name}" for asset in assets) + "\n",
                encoding="ascii",
            )
            provenance = release_dir / "provenance.jsonl"
            provenance.write_text("{}\n", encoding="ascii")
            manifest = release_dir / "RELEASE-MANIFEST.json"
            manifest.write_text(
                json.dumps(
                    {
                        "assets": [
                            {
                                "name": asset.name,
                                "size": asset.size,
                                "sha256": asset.sha256,
                            }
                            for asset in assets
                        ],
                        "checksums": {"name": checksums.name},
                        "provenance": {
                            "name": provenance.name,
                            "size": provenance.stat().st_size,
                            "sha256": hashlib.sha256(provenance.read_bytes()).hexdigest(),
                        },
                    }
                ),
                encoding="utf-8",
            )
            expected = publisher.load_manifest(release_dir, manifest)
            self.assertEqual(
                {asset.name for asset in expected},
                {asset.name for asset in assets}
                | {manifest.name, checksums.name, provenance.name},
            )


class GitHubApiTests(unittest.TestCase):
    def test_ruleset_summaries_are_resolved_to_security_relevant_details(self):
        api = publisher.GitHubApi(
            "owner/repository", "fixture-token", policy_token="fixture-policy-token"
        )
        api._request = mock.Mock(
            side_effect=[
                [{"id": 42, "name": "Protect release tags", "target": "tag"}],
                {
                    "id": 42,
                    "target": "tag",
                    "enforcement": "active",
                    "conditions": {
                        "ref_name": {"include": ["refs/tags/v*"], "exclude": []}
                    },
                    "rules": [{"type": "deletion"}, {"type": "update"}],
                    "bypass_actors": [],
                },
            ]
        )
        rulesets = api.tag_rulesets()
        self.assertEqual(len(rulesets), 1)
        self.assertTrue(rulesets[0].protects("v0.189.17"))
        self.assertEqual(
            api._request.call_args_list[1],
            mock.call(
                "GET",
                "/repos/owner/repository/rulesets/42?includes_parents=true",
                policy=True,
            ),
        )
        self.assertEqual(
            api._request.call_args_list[0],
            mock.call(
                "GET",
                "/repos/owner/repository/rulesets?includes_parents=true&targets=tag&per_page=100&page=1",
                policy=True,
            ),
        )

    def test_immutable_setting_uses_only_the_policy_token_path(self):
        api = publisher.GitHubApi(
            "owner/repository", "fixture-token", policy_token="fixture-policy-token"
        )
        api._request = mock.Mock(return_value={"enabled": True})
        self.assertTrue(api.immutable_releases_enabled())
        api._request.assert_called_once_with(
            "GET", "/repos/owner/repository/immutable-releases", policy=True
        )

    def test_undisclosed_bypass_field_fails_closed(self):
        ruleset = publisher.TagRuleset.from_json(
            {
                "target": "tag",
                "enforcement": "active",
                "conditions": {
                    "ref_name": {"include": ["refs/tags/v*"], "exclude": []}
                },
                "rules": [{"type": "deletion"}, {"type": "update"}],
            }
        )
        self.assertIsNone(ruleset.bypass_actors)
        self.assertFalse(ruleset.protects("v0.189.17"))

    def test_upload_refuses_an_unexpected_host_before_reading_the_asset(self):
        api = publisher.GitHubApi(
            "owner/repository", "fixture-token", policy_token="fixture-policy-token"
        )
        release = publisher.Release(
            id=1,
            tag_name="v0.189.17",
            draft=True,
            prerelease=False,
            immutable=False,
            assets=(),
            upload_url="https://attacker.example/repos/owner/repository/releases/1/assets{?name}",
        )
        asset = mock.Mock()
        with self.assertRaisesRegex(publisher.ApiError, "unexpected release upload URL"):
            api.upload_asset(release, asset)
        asset.verify_local.assert_not_called()


class PublicationStateMachineTests(unittest.TestCase):
    def test_empty_release_uploads_exact_assets_and_publishes_latest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            result = make_publisher(root, api).publish()
            self.assertFalse(result.idempotent)
            self.assertTrue(result.make_latest)
            self.assertEqual(
                [event[0] for event in api.events],
                [
                    "immutable",
                    "rulesets",
                    "tag",
                    "get",
                    "create",
                    "upload",
                    "upload",
                    "get",
                    "list",
                    "immutable",
                    "rulesets",
                    "tag",
                    "publish",
                    "tag",
                ],
            )
            self.assertTrue(api.releases[api.tag].immutable)

    def test_lower_version_is_published_without_latest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi((), tag="v0.189.15")
            api.stable = [
                publisher.Release(
                    id=5,
                    tag_name="v0.189.16",
                    draft=False,
                    prerelease=False,
                    immutable=True,
                    assets=(),
                )
            ]
            result = make_publisher(root, api).publish()
            self.assertFalse(result.make_latest)
            self.assertEqual(
                [event for event in api.events if event[0] == "publish"],
                [("publish", False)],
            )
            self.assertEqual(api.events[-1], ("tag", api.tag))

    def test_partial_draft_uploads_only_missing_assets_without_clobber(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            release_dir, manifest, assets = load_assets(root)
            api.assets = assets
            api.releases[api.tag] = publisher.Release(
                id=9,
                tag_name=api.tag,
                draft=True,
                prerelease=False,
                immutable=False,
                assets=(
                    publisher.ReleaseAsset(
                        assets[0].name,
                        assets[0].size,
                        assets[0].sha256,
                    ),
                ),
                upload_url="fixture://upload",
            )
            result = publisher.ReleasePublisher(
                api,
                tag=api.tag,
                expected_commit=COMMIT,
                title="Forge",
                assets=assets,
            ).publish()
            self.assertFalse(result.idempotent)
            self.assertEqual(
                [value for key, value in api.events if key == "upload"],
                [assets[1].name],
            )

    def test_exact_published_immutable_release_is_idempotent_and_read_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            _, _, assets = load_assets(root)
            api.assets = assets
            api.releases[api.tag] = release_with_assets(api, draft=False, immutable=True)
            before = len(api.events)
            result = make_publisher(root, api).publish()
            self.assertTrue(result.idempotent)
            self.assertIsNone(result.make_latest)
            self.assertNotIn("publish", [key for key, _ in api.events[before:]])

    def test_extra_draft_asset_is_rejected_without_upload_or_delete(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            _, _, assets = load_assets(root)
            api.assets = assets
            api.releases[api.tag] = release_with_assets(
                api,
                draft=True,
                immutable=False,
                extra=(publisher.ReleaseAsset("evil.bin", 3, "b" * 64),),
            )
            with self.assertRaisesRegex(publisher.PublicationError, "unlisted"):
                make_publisher(root, api).publish()
            self.assertNotIn("upload", [key for key, _ in api.events])

    def test_mismatched_draft_digest_is_rejected_without_clobber(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            _, _, assets = load_assets(root)
            api.assets = assets
            api.releases[api.tag] = replace(
                release_with_assets(api, draft=True, immutable=False),
                assets=(publisher.ReleaseAsset(assets[0].name, assets[0].size, "c" * 64),),
            )
            with self.assertRaisesRegex(publisher.PublicationError, "digest mismatch"):
                make_publisher(root, api).publish()
            self.assertNotIn("upload", [key for key, _ in api.events])

    def test_published_nonimmutable_release_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            _, _, assets = load_assets(root)
            api.assets = assets
            api.releases[api.tag] = release_with_assets(api, draft=False, immutable=False)
            with self.assertRaisesRegex(publisher.PublicationError, "not immutable"):
                make_publisher(root, api).publish()

    def test_wrong_tag_commit_is_rejected_before_draft_creation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            api.tag_commit = "b" * 40
            with self.assertRaisesRegex(publisher.PublicationError, "resolves"):
                make_publisher(root, api).publish()
            self.assertNotIn("create", [key for key, _ in api.events])

    def test_lightweight_tag_is_rejected_before_draft_creation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            api.tag_annotated = False
            with self.assertRaisesRegex(publisher.PublicationError, "annotated"):
                make_publisher(root, api).publish()
            self.assertNotIn("create", [key for key, _ in api.events])

    def test_missing_immutable_setting_and_unprotected_ruleset_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            api.immutable = False
            with self.assertRaisesRegex(publisher.PublicationError, "immutable"):
                make_publisher(root, api).publish()

            api.immutable = True
            api.rulesets = []
            with self.assertRaisesRegex(publisher.PublicationError, "ruleset"):
                make_publisher(root, api).publish()

    def test_publish_timeout_reconciles_successfully_without_retrying_publish(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            api.publish_timeout = True
            result = make_publisher(root, api).publish()
            self.assertTrue(result.idempotent)
            self.assertEqual(
                [key for key, _ in api.events].count("publish"),
                1,
            )

    def test_publish_timeout_with_draft_state_fails_without_second_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            api.publish_timeout_before_commit = True
            with self.assertRaisesRegex(publisher.PublicationError, "remains a draft"):
                make_publisher(root, api).publish()
            self.assertEqual(
                [key for key, _ in api.events].count("publish"),
                1,
            )

    def test_publish_server_error_reconciles_without_retrying_patch(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            api.publish_server_error = True
            result = make_publisher(root, api).publish()
            self.assertTrue(result.idempotent)
            self.assertEqual(
                [key for key, _ in api.events].count("publish"),
                1,
            )

    def test_tag_ruleset_with_bypass_actor_is_not_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())
            api.rulesets = [replace(api.rulesets[0], bypass_actors=(object(),))]
            with self.assertRaisesRegex(publisher.PublicationError, "ruleset"):
                make_publisher(root, api).publish()

    def test_repository_guards_are_rechecked_before_publish(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            api = FixtureApi(())

            def immutable_after_preflight():
                api.events.append(("immutable", None))
                api.immutable_checks += 1
                return api.immutable_checks < 2

            api.immutable_releases_enabled = immutable_after_preflight
            with self.assertRaisesRegex(publisher.PublicationError, "immutable"):
                make_publisher(root, api).publish()
            self.assertNotIn("publish", [key for key, _ in api.events])
if __name__ == "__main__":
    unittest.main()
