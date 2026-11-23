#!/usr/bin/env python3
"""Validate and enrich the SBOM for one sendmer release archive.

The release archive is deliberately tiny. It must contain exactly the target
binary and BUILD-METADATA.txt. Syft scans the unpacked directory before the
archive is created, so this script verifies that the resulting SPDX file still
covers the exact bytes shipped in the archive. It also records the final
archive SHA-256 in the SPDX root package.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import tarfile
import zipfile
from typing import Any


def normalize(name: str) -> str:
    value = name.replace("\\", "/").lstrip("/")
    while value.startswith("./"):
        value = value[2:]
    if ".." in pathlib.PurePosixPath(value).parts:
        raise ValueError(f"archive contains a parent traversal path: {name}")
    return value


def read_archive(path: pathlib.Path) -> dict[str, bytes]:
    members: dict[str, bytes] = {}
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            for item in archive.infolist():
                if not item.is_dir():
                    name = normalize(item.filename)
                    if name in members:
                        raise ValueError(f"archive contains a duplicate member: {name}")
                    members[name] = archive.read(item)
    else:
        with tarfile.open(path, "r:gz") as archive:
            for item in archive.getmembers():
                if item.isdir():
                    continue
                if not item.isfile():
                    raise ValueError(f"archive contains a non-regular file: {item.name}")
                stream = archive.extractfile(item)
                if stream is None:
                    raise ValueError(f"cannot read archive member: {item.name}")
                name = normalize(item.name)
                if name in members:
                    raise ValueError(f"archive contains a duplicate member: {name}")
                members[name] = stream.read()
    return members


def checksum_for(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def validate_checksum(path: pathlib.Path, checksum_path: pathlib.Path, digest: str) -> None:
    fields = checksum_path.read_text(encoding="utf-8").split()
    if len(fields) < 2 or fields[0].lower() != digest:
        raise ValueError("archive checksum file does not match the archive")
    if fields[1] != path.name:
        raise ValueError("archive checksum file names the wrong archive")


def validate_and_enrich(
    archive_path: pathlib.Path,
    checksum_path: pathlib.Path,
    sbom_path: pathlib.Path,
    repository: str,
    version: str,
) -> str:
    digest = checksum_for(archive_path)
    validate_checksum(archive_path, checksum_path, digest)

    members = read_archive(archive_path)
    expected_binary = "sendmer.exe" if archive_path.suffix == ".zip" else "sendmer"
    expected_members = {expected_binary, "BUILD-METADATA.txt"}
    if set(members) != expected_members:
        raise ValueError(
            f"archive members {sorted(members)} do not equal {sorted(expected_members)}"
        )

    document: dict[str, Any] = json.loads(sbom_path.read_text(encoding="utf-8"))

    # Syft can omit ordinary metadata files and can emit zero SHA-1
    # placeholders for files discovered in a directory. Normalize the SPDX
    # file inventory from the bytes that are actually shipped in the archive.
    # This makes the SBOM a complete, independently checkable manifest.
    existing_files = {
        normalize(item["fileName"]): item
        for item in document.get("files", [])
        if item.get("fileName")
    }
    sbom_files: dict[str, dict[str, Any]] = {}
    for name in sorted(expected_members):
        item = existing_files.get(name, {})
        file_id = item.get("SPDXID") or f"SPDXRef-File-{hashlib.sha1(name.encode()).hexdigest()[:16]}"
        item.update(
            {
                "fileName": name,
                "SPDXID": file_id,
                "checksums": [
                    {
                        "algorithm": "SHA1",
                        "checksumValue": hashlib.sha1(members[name]).hexdigest(),
                    },
                    {
                        "algorithm": "SHA256",
                        "checksumValue": hashlib.sha256(members[name]).hexdigest(),
                    },
                ],
                "licenseConcluded": item.get("licenseConcluded", "NOASSERTION"),
                "licenseInfoInFiles": item.get("licenseInfoInFiles", ["NOASSERTION"]),
                "copyrightText": item.get("copyrightText", "NOASSERTION"),
            }
        )
        sbom_files[name] = item
    document["files"] = [sbom_files[name] for name in sorted(sbom_files)]

    described_ids = {
        relationship["relatedSpdxElement"]
        for relationship in document.get("relationships", [])
        if relationship.get("relationshipType") == "DESCRIBES"
    }
    roots = [
        package
        for package in document.get("packages", [])
        if package.get("SPDXID") in described_ids
    ]
    if len(roots) != 1:
        raise ValueError("SBOM must contain exactly one described root package")

    root = roots[0]
    root["filesAnalyzed"] = True
    root["checksums"] = [{"algorithm": "SHA256", "checksumValue": digest}]
    root["downloadLocation"] = (
        f"https://github.com/{repository}/releases/download/{version}/{archive_path.name}"
    )
    relationships = document.setdefault("relationships", [])
    for item in document["files"]:
        if not any(
            relationship.get("relationshipType") == "CONTAINS"
            and relationship.get("spdxElementId") == root["SPDXID"]
            and relationship.get("relatedSpdxElement") == item["SPDXID"]
            for relationship in relationships
        ):
            relationships.append(
                {
                    "spdxElementId": root["SPDXID"],
                    "relatedSpdxElement": item["SPDXID"],
                    "relationshipType": "CONTAINS",
                }
            )
    sbom_path.write_text(
        json.dumps(document, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        + "\n",
        encoding="utf-8",
    )
    return digest


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("archive", type=pathlib.Path)
    parser.add_argument("checksum", type=pathlib.Path)
    parser.add_argument("sbom", type=pathlib.Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    digest = validate_and_enrich(
        args.archive,
        args.checksum,
        args.sbom,
        args.repository,
        args.version,
    )
    print(
        f"validated {args.archive.name}: archive sha256={digest}",
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
