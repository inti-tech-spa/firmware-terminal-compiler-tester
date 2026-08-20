#!/usr/bin/env python3
"""Validate samdebug-specific SPDX invariants that generic JSON parsing misses."""

import argparse
import json
import re


LICENSE_REF = re.compile(r"\bLicenseRef-[A-Za-z0-9.-]+\b")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("sbom")
    args = parser.parse_args()
    with open(args.sbom, encoding="utf-8") as source:
        document = json.load(source)

    if document.get("spdxVersion") != "SPDX-2.3":
        raise SystemExit("expected SPDX-2.3 document")

    extracted = {
        item["licenseId"]: item
        for item in document.get("hasExtractedLicensingInfos", [])
    }
    referenced = set()
    for package in document.get("packages", []):
        expression = package.get("licenseDeclared", "")
        referenced.update(LICENSE_REF.findall(expression))
        if package.get("name") == "openocd" and not (
            "(BSD-3-Clause OR GPL-3.0-only)" in expression
        ):
            raise SystemExit("OpenOCD alternative license must remain parenthesized")

    missing = referenced - extracted.keys()
    if missing:
        raise SystemExit(f"undefined extracted license references: {sorted(missing)}")
    for license_id in referenced:
        info = extracted[license_id]
        if not info.get("name") or not info.get("extractedText"):
            raise SystemExit(f"incomplete extracted license definition: {license_id}")

    unused = extracted.keys() - referenced
    if unused:
        raise SystemExit(f"unused extracted license definitions: {sorted(unused)}")


if __name__ == "__main__":
    main()
