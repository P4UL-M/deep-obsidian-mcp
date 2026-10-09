#!/usr/bin/env python3
"""Validate the released source and derive registry tags without shell interpolation."""
import os
import re
import tomllib
from pathlib import Path


def metadata(tag, version, repository, dockerhub_username, prerelease=False):
    match = re.fullmatch(r"v(\d+\.\d+\.\d+)(?:-([0-9A-Za-z.-]+))?", tag)
    if not match or tag[1:] != version:
        raise ValueError("release tag must be v<workspace version> and a Docker-compatible version")
    if not re.fullmatch(r"[a-z0-9]+(?:[a-z0-9_-]*[a-z0-9])?", dockerhub_username):
        raise ValueError("set DOCKERHUB_USERNAME to a lowercase Docker Hub account or organization")
    repository = repository.lower()
    if not re.fullmatch(r"[a-z0-9_.-]+/[a-z0-9_.-]+", repository):
        raise ValueError("invalid GitHub repository")
    suffix = match[2]
    channel = "latest"
    if suffix or prerelease:
        prefix = suffix.split(".")[0] if suffix else ""
        channel = prefix if prefix in {"alpha", "beta", "rc"} else "preview"
    return {"version": version, "tag": tag, "channel": channel,
            "ghcr": f"ghcr.io/{repository}",
            "dockerhub": f"docker.io/{dockerhub_username}/deep-obsidian-mcp"}


if __name__ == "__main__":
    with Path("Cargo.toml").open("rb") as file:
        version = tomllib.load(file)["workspace"]["package"]["version"]
    values = metadata(os.environ["RELEASE_TAG"], version, os.environ["GITHUB_REPOSITORY"],
                      os.environ.get("DOCKERHUB_USERNAME", ""),
                      os.environ.get("RELEASE_PRERELEASE", "false") == "true")
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        for key, value in values.items():
            print(f"{key}={value}", file=output)
