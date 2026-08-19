#!/usr/bin/env python3
import json
import pathlib
import sys


def package(name: str, version: str, license_id: str, download: str) -> dict:
    return {
        "SPDXID": f"SPDXRef-Package-{name}",
        "name": name,
        "versionInfo": version,
        "downloadLocation": download,
        "filesAnalyzed": False,
        "licenseConcluded": license_id,
        "licenseDeclared": license_id,
        "copyrightText": "NOASSERTION",
    }


destination, openocd, libusb, hidapi, jimtcl = sys.argv[1:6]
document = {
    "spdxVersion": "SPDX-2.3",
    "dataLicense": "CC0-1.0",
    "SPDXID": "SPDXRef-DOCUMENT",
    "name": f"samdebug-openocd-{openocd}-darwin-arm64",
    "documentNamespace": f"https://inti.tech/samdebug/sbom/openocd-{openocd}-darwin-arm64",
    "creationInfo": {
        "created": "2026-08-19T00:00:00Z",
        "creators": ["Organization: Inti Tech SPA", "Tool: samdebug-build-recipe"],
    },
    "packages": [
        package("OpenOCD", openocd, "GPL-2.0-or-later", "https://sourceforge.net/projects/openocd/files/openocd/0.12.0/"),
        package("libusb", libusb, "LGPL-2.1-or-later", "https://github.com/libusb/libusb/releases"),
        package("hidapi", hidapi, "BSD-3-Clause OR GPL-3.0-only", "https://github.com/libusb/hidapi/releases"),
        package("JimTcl", jimtcl, "BSD-2-Clause", "https://github.com/msteveb/jimtcl"),
    ],
    "relationships": [
        {"spdxElementId": "SPDXRef-DOCUMENT", "relationshipType": "DESCRIBES", "relatedSpdxElement": "SPDXRef-Package-OpenOCD"},
        {"spdxElementId": "SPDXRef-Package-OpenOCD", "relationshipType": "DEPENDS_ON", "relatedSpdxElement": "SPDXRef-Package-libusb"},
        {"spdxElementId": "SPDXRef-Package-OpenOCD", "relationshipType": "DEPENDS_ON", "relatedSpdxElement": "SPDXRef-Package-hidapi"},
        {"spdxElementId": "SPDXRef-Package-OpenOCD", "relationshipType": "DEPENDS_ON", "relatedSpdxElement": "SPDXRef-Package-JimTcl"},
    ],
}
pathlib.Path(destination).write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
