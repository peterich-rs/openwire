import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parent.parent
RELEASE_VERSION = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


class ReleaseError(Exception):
    pass


def publishable_packages():
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
    )
    return {
        package["name"]: package["version"]
        for package in json.loads(result.stdout)["packages"]
        if package["publish"] is None or "crates-io" in package["publish"]
    }


def unique_fields(pairs):
    fields = {}
    for name, value in pairs:
        if name in fields:
            raise ReleaseError(f"duplicate release plan field {name!r}")
        fields[name] = value
    return fields


def read_release(path, requested_version, packages):
    with path.open(encoding="utf-8") as handle:
        plan = json.load(handle, object_pairs_hook=unique_fields)
    if not isinstance(plan, dict) or plan.keys() != {"version", "crates"}:
        raise ReleaseError(f"{path.name} must contain version and crates")

    release_version = plan["version"]
    if not isinstance(release_version, str) or not RELEASE_VERSION.fullmatch(release_version):
        raise ReleaseError(f"invalid release version in {path.name}")
    if requested_version is not None and requested_version != release_version:
        raise ReleaseError(f"requested release {requested_version!r} has no plan in {path.name}")

    selection = plan["crates"]
    if not isinstance(selection, dict) or not selection:
        raise ReleaseError(f"{path.name} must select at least one crate")
    for name, version in selection.items():
        if name not in packages:
            raise ReleaseError(f"{name} is not a crates.io publishable workspace crate")
        if version != packages[name]:
            raise ReleaseError(f"{name} plan selects {version!r}, workspace has {packages[name]}")
    return release_version, selection


def index_path(name):
    name = name.lower()
    if len(name) <= 2:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


def registry_version_exists(name, version):
    request = Request(
        f"https://index.crates.io/{index_path(name)}",
        headers={"User-Agent": "OpenWire release tooling", "Cache-Control": "no-cache"},
    )
    try:
        with urlopen(request, timeout=30) as response:
            entries = [json.loads(line) for line in response if line.strip()]
    except HTTPError as error:
        if error.code in (404, 410):
            return False
        raise ReleaseError(f"crates.io index lookup failed for {name}: HTTP {error.code}") from error
    except (URLError, OSError, ValueError) as error:
        raise ReleaseError(f"crates.io index lookup failed for {name}: {error}") from error

    if not entries or any(
        not isinstance(entry, dict)
        or entry.get("name") != name
        or not isinstance(entry.get("vers"), str)
        or not isinstance(entry.get("yanked"), bool)
        for entry in entries
    ):
        raise ReleaseError(f"invalid crates.io index entry for {name}")

    for entry in entries:
        published_version = entry["vers"]
        if published_version.partition("+")[0] != version.partition("+")[0]:
            continue
        if published_version != version:
            raise ReleaseError(f"{name}@{published_version} occupies {name}@{version} on crates.io")
        if entry["yanked"]:
            raise ReleaseError(f"{name}@{version} is yanked on crates.io")
        return True
    return False


def pending_packages(selection):
    pending = {}
    for name, version in selection.items():
        if registry_version_exists(name, version):
            print(f"Already published: {name}@{version}", flush=True)
        else:
            pending[name] = version
    return pending


def run_cargo(command, selection, allow_dirty):
    args = ["cargo", command, "--locked", "--registry", "crates-io"]
    if command == "package":
        args.append("--all-features")
    if allow_dirty:
        args.append("--allow-dirty")
    for name, version in selection.items():
        args.extend(["-p", f"{name}@{version}"])
    subprocess.run(args, cwd=ROOT, check=True)


def verify_visibility(selection):
    waiting = selection
    for _ in range(12):
        waiting = {
            name: version
            for name, version in waiting.items()
            if not registry_version_exists(name, version)
        }
        if not waiting:
            return
        time.sleep(5)
    versions = ", ".join(f"{name}@{version}" for name, version in waiting.items())
    raise ReleaseError(f"crates.io index has not picked up {versions}; rerun this release after it updates")


def main():
    parser = argparse.ArgumentParser(description="Package or publish the batch in release.json.")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true", help="package and verify without uploading")
    mode.add_argument("--publish", action="store_true", help="upload pending versions to crates.io")
    parser.add_argument("--release", help="release identifier from the Git tag, without the v prefix")
    parser.add_argument("--allow-dirty", action="store_true", help="allow modified package files")
    args = parser.parse_args()
    if args.publish and args.release is None:
        parser.error("--publish requires --release")

    release_version, selection = read_release(ROOT / "release.json", args.release, publishable_packages())
    print(f"OpenWire release {release_version}", flush=True)
    if args.dry_run:
        run_cargo("package", selection, args.allow_dirty)
    else:
        pending = pending_packages(selection)
        if pending:
            run_cargo("publish", pending, args.allow_dirty)
            verify_visibility(pending)
        else:
            print("All planned versions are already published.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (ReleaseError, OSError, json.JSONDecodeError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
