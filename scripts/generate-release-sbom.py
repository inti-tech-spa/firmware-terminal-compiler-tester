#!/usr/bin/env python3
"""Generate a deterministic SPDX 2.3 JSON SBOM for the Rust release binary."""

import argparse
import datetime
import hashlib
import json
import os
import subprocess
import uuid
from typing import Optional


EXTRACTED_LICENSES = {
    "LicenseRef-Arm-GNU-Manifest": {
        "name": "Arm GNU Toolchain build manifest",
        "extractedText": (
            "Arm GNU Toolchain 15.2.Rel1 build-manifest notice. The packaged "
            "15.2.rel1-darwin-arm64-arm-none-eabi-manifest.txt records the "
            "configure options and component revisions used to produce the "
            "redistributed binary toolchain. It is distributed verbatim with "
            "the managed tool bundle and is not a substitute for the component "
            "license texts in that bundle."
        ),
    },
    "LicenseRef-Arm-GNU-Release-Notices": {
        "name": "Arm GNU Toolchain release notice",
        "extractedText": (
            "Arm GNU Toolchain 15.2.rel1\nGCC Version: 15.2\n\n"
            "For updated content, see the release note for the relevant release, on:\n"
            "https://developer.arm.com/downloads/-/arm-gnu-toolchain-downloads"
        ),
    },
    "LicenseRef-JimTcl-Tcl": {
        "name": "Jim Tcl license terms",
        "extractedText": (
            "This software is copyrighted by the Regents of the University of\n"
            "California, Sun Microsystems, Inc., Scriptics Corporation, ActiveState\n"
            "Corporation and other parties.  The following terms apply to all files\n"
            "associated with the software unless explicitly disclaimed in\n"
            "individual files.\n\n"
            "The authors hereby grant permission to use, copy, modify, distribute,\n"
            "and license this software and its documentation for any purpose, provided\n"
            "that existing copyright notices are retained in all copies and that this\n"
            "notice is included verbatim in any distributions. No written agreement,\n"
            "license, or royalty fee is required for any of the authorized uses.\n"
            "Modifications to this software may be copyrighted by their authors\n"
            "and need not follow the licensing terms described here, provided that\n"
            "the new terms are clearly indicated on the first page of each file where\n"
            "they apply.\n\n"
            "IN NO EVENT SHALL THE AUTHORS OR DISTRIBUTORS BE LIABLE TO ANY PARTY\n"
            "FOR DIRECT, INDIRECT, SPECIAL, INCIDENTAL, OR CONSEQUENTIAL DAMAGES\n"
            "ARISING OUT OF THE USE OF THIS SOFTWARE, ITS DOCUMENTATION, OR ANY\n"
            "DERIVATIVES THEREOF, EVEN IF THE AUTHORS HAVE BEEN ADVISED OF THE\n"
            "POSSIBILITY OF SUCH DAMAGE.\n\n"
            "THE AUTHORS AND DISTRIBUTORS SPECIFICALLY DISCLAIM ANY WARRANTIES,\n"
            "INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY,\n"
            "FITNESS FOR A PARTICULAR PURPOSE, AND NON-INFRINGEMENT.  THIS SOFTWARE\n"
            "IS PROVIDED ON AN \"AS IS\" BASIS, AND THE AUTHORS AND DISTRIBUTORS HAVE\n"
            "NO OBLIGATION TO PROVIDE MAINTENANCE, SUPPORT, UPDATES, ENHANCEMENTS, OR\n"
            "MODIFICATIONS.\n\n"
            "GOVERNMENT USE: If you are acquiring this software on behalf of the\n"
            "U.S. government, the Government shall have only \"Restricted Rights\"\n"
            "in the software and related documentation as defined in the Federal\n"
            "Acquisition Regulations (FARs) in Clause 52.227.19 (c) (2).  If you\n"
            "are acquiring the software on behalf of the Department of Defense, the\n"
            "software shall be classified as \"Commercial Computer Software\" and the\n"
            "Government shall have only \"Restricted Rights\" as defined in Clause\n"
            "252.227-7013 (c) (1) of DFARs.  Notwithstanding the foregoing, the\n"
            "authors grant the U.S. Government and others acting in its behalf\n"
            "permission to use and distribute the software in accordance with the\n"
            "terms specified in this license."
        ),
    },
}


def spdx_id(package_id: str) -> str:
    digest = hashlib.sha256(package_id.encode()).hexdigest()[:24]
    return f"SPDXRef-Package-{digest}"


def normalize_license_expression(expression: Optional[str]) -> str:
    """Normalize Cargo's legacy slash-separated alternatives to SPDX syntax."""
    if not expression:
        return "NOASSERTION"
    if "/" not in expression:
        return expression
    return " OR ".join(part.strip() for part in expression.split("/") if part.strip())


def reachable_package_ids(root_id: str, nodes: list[dict]) -> set[str]:
    dependencies = {node["id"]: node.get("dependencies", []) for node in nodes}
    reachable = set()
    pending = [root_id]
    while pending:
        package_id = pending.pop()
        if package_id in reachable:
            continue
        reachable.add(package_id)
        pending.extend(dependencies.get(package_id, []))
    return reachable


def cargo_download_location(package: dict) -> str:
    source = package.get("source") or ""
    if source.startswith("registry+"):
        return (
            f"https://crates.io/api/v1/crates/{package['name']}/"
            f"{package['version']}/download"
        )
    if source.startswith("git+"):
        return source.removeprefix("git+")
    return package.get("repository") or source or "NOASSERTION"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cargo", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--tool-manifest", required=True)
    args = parser.parse_args()
    metadata = json.loads(
        subprocess.check_output(
            [
                args.cargo,
                "metadata",
                "--locked",
                "--format-version",
                "1",
                "--filter-platform",
                "aarch64-apple-darwin",
            ],
            text=True,
        )
    )
    packages_by_id = {package["id"]: package for package in metadata["packages"]}
    root_id = next(
        package["id"]
        for package in metadata["packages"]
        if package["name"] == "samdebug" and package["source"] is None
    )
    root = packages_by_id[root_id]
    resolve = metadata.get("resolve") or {}
    resolve_nodes = resolve.get("nodes", [])
    reachable = reachable_package_ids(root_id, resolve_nodes)
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
        "hasExtractedLicensingInfos": [
            {"licenseId": license_id, **EXTRACTED_LICENSES[license_id]}
            for license_id in sorted(EXTRACTED_LICENSES)
        ],
    }
    for package_id, package in sorted(
        (
            (package_id, packages_by_id[package_id])
            for package_id in reachable
            if package_id in packages_by_id
        ),
        key=lambda item: (item[1]["name"], item[1]["version"], item[0]),
    ):
        license_value = normalize_license_expression(package.get("license"))
        download = cargo_download_location(package)
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
            "licenseDeclared": normalize_license_expression(root.get("license")),
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
    for node in sorted(resolve_nodes, key=lambda value: value["id"]):
        if node["id"] not in reachable:
            continue
        for dependency in sorted(node.get("dependencies", [])):
            if dependency not in reachable:
                continue
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
        expressions = sorted(
            {license_entry["spdx"] for license_entry in artifact["licenses"]}
        )
        declared = " AND ".join(
            f"({expression})" if " OR " in expression else expression
            for expression in expressions
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
