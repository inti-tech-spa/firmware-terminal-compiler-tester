#!/usr/bin/env python3
"""Generate a deterministic SPDX 2.3 JSON SBOM for the Rust release binary."""

import argparse
import datetime
import hashlib
import json
import os
import subprocess
import uuid


def spdx_id(package_id: str) -> str:
    digest = hashlib.sha256(package_id.encode()).hexdigest()[:24]
    return f"SPDXRef-Package-{digest}"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cargo", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--tool-manifest", required=True)
    args = parser.parse_args()
    metadata = json.loads(
        subprocess.check_output(
            [args.cargo, "metadata", "--locked", "--format-version", "1"], text=True
        )
    )
    packages_by_id = {package["id"]: package for package in metadata["packages"]}
    root_id = next(
        package["id"]
        for package in metadata["packages"]
        if package["name"] == "samdebug" and package["source"] is None
    )
    root = packages_by_id[root_id]
    with open(args.binary, "rb") as binary:
        binary_hash = hashlib.sha256(binary.read()).hexdigest()
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    created = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    namespace_seed = f"samdebug-{root['version']}-{binary_hash}"
    document = {
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": f"samdebug-{root['version']}-macos-arm64",
        "documentNamespace": f"https://inti.tech/samdebug/spdx/{uuid.uuid5(uuid.NAMESPACE_URL, namespace_seed)}",
        "creationInfo": {
            "created": created,
            "creators": ["Tool: samdebug-generate-release-sbom.py"],
        },
        "packages": [],
        "relationships": [],
    }
    for package_id, package in sorted(
        packages_by_id.items(), key=lambda item: (item[1]["name"], item[1]["version"], item[0])
    ):
        license_value = package.get("license") or "NOASSERTION"
        download = package.get("source") or package.get("repository") or "NOASSERTION"
        document["packages"].append(
            {
                "name": package["name"],
                "SPDXID": spdx_id(package_id),
                "versionInfo": package["version"],
                "downloadLocation": download,
                "filesAnalyzed": False,
                "licenseConcluded": "NOASSERTION",
                "licenseDeclared": license_value,
                "copyrightText": "NOASSERTION",
            }
        )
    document["packages"].append(
        {
            "name": "samdebug-macos-arm64-binary",
            "SPDXID": "SPDXRef-ReleaseBinary",
            "versionInfo": root["version"],
            "downloadLocation": "NOASSERTION",
            "filesAnalyzed": False,
            "checksums": [{"algorithm": "SHA256", "checksumValue": binary_hash}],
            "licenseConcluded": "NOASSERTION",
            "licenseDeclared": root.get("license") or "NOASSERTION",
            "copyrightText": "NOASSERTION",
        }
    )
    document["relationships"].append(
        {
            "spdxElementId": "SPDXRef-DOCUMENT",
            "relationshipType": "DESCRIBES",
            "relatedSpdxElement": "SPDXRef-ReleaseBinary",
        }
    )
    document["relationships"].append(
        {
            "spdxElementId": "SPDXRef-ReleaseBinary",
            "relationshipType": "GENERATED_FROM",
            "relatedSpdxElement": spdx_id(root_id),
        }
    )
    resolve = metadata.get("resolve") or {}
    for node in sorted(resolve.get("nodes", []), key=lambda value: value["id"]):
        for dependency in sorted(node.get("dependencies", [])):
            document["relationships"].append(
                {
                    "spdxElementId": spdx_id(node["id"]),
                    "relationshipType": "DEPENDS_ON",
                    "relatedSpdxElement": spdx_id(dependency),
                }
            )
    with open(args.tool_manifest, encoding="utf-8") as manifest_file:
        tool_manifest = json.load(manifest_file)
    for artifact in sorted(tool_manifest["artifacts"], key=lambda value: value["name"]):
        artifact_identity = f"managed:{artifact['name']}:{artifact['version']}"
        artifact_spdx = spdx_id(artifact_identity)
        source_spdx = spdx_id(f"source:{artifact_identity}")
        declared = " AND ".join(
            sorted({license_entry["spdx"] for license_entry in artifact["licenses"]})
        )
        document["packages"].append(
            {
                "name": artifact["name"],
                "SPDXID": artifact_spdx,
                "versionInfo": artifact["version"],
                "downloadLocation": artifact["url"],
                "filesAnalyzed": False,
                "checksums": [
                    {"algorithm": "SHA256", "checksumValue": artifact["sha256"]}
                ],
                "licenseConcluded": "NOASSERTION",
                "licenseDeclared": declared,
                "copyrightText": "NOASSERTION",
                "primaryPackagePurpose": "APPLICATION",
            }
        )
        document["packages"].append(
            {
                "name": f"{artifact['name']}-corresponding-source",
                "SPDXID": source_spdx,
                "versionInfo": artifact["version"],
                "downloadLocation": artifact["source_url"],
                "filesAnalyzed": False,
                "checksums": [
                    {
                        "algorithm": "SHA256",
                        "checksumValue": artifact["source_sha256"],
                    }
                ],
                "licenseConcluded": "NOASSERTION",
                "licenseDeclared": declared,
                "copyrightText": "NOASSERTION",
                "primaryPackagePurpose": "SOURCE",
            }
        )
        document["relationships"].extend(
            [
                {
                    "spdxElementId": "SPDXRef-ReleaseBinary",
                    "relationshipType": "DEPENDS_ON",
                    "relatedSpdxElement": artifact_spdx,
                },
                {
                    "spdxElementId": artifact_spdx,
                    "relationshipType": "GENERATED_FROM",
                    "relatedSpdxElement": source_spdx,
                },
            ]
        )
    document["packages"].sort(key=lambda package: package["SPDXID"])
    document["relationships"].sort(
        key=lambda relationship: (
            relationship["spdxElementId"],
            relationship["relationshipType"],
            relationship["relatedSpdxElement"],
        )
    )
    with open(args.output, "w", encoding="utf-8") as output:
        json.dump(document, output, indent=2, sort_keys=True)
        output.write("\n")


if __name__ == "__main__":
    main()
