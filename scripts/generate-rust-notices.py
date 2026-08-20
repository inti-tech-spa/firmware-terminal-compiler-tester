#!/usr/bin/env python3
"""Generate concise third-party Rust package notices from locked Cargo metadata."""

import argparse
import hashlib
import json
import subprocess
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cargo", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--licenses-output", required=True)
    args = parser.parse_args()
    metadata = json.loads(
        subprocess.check_output(
            [args.cargo, "metadata", "--locked", "--format-version", "1"], text=True
        )
    )
    packages = [package for package in metadata["packages"] if package["source"]]
    packages.sort(key=lambda package: (package["name"], package["version"], package["id"]))
    with open(args.output, "w", encoding="utf-8") as output:
        output.write("# samdebug third-party notices\n\n")
        output.write(
            "This release includes the following locked Rust packages. License texts are "
            "available from each package's source distribution and the Cargo registry cache.\n\n"
        )
        for package in packages:
            license_value = package.get("license") or "NOASSERTION"
            source = package.get("repository") or package.get("homepage") or package["source"]
            output.write(
                f"- {package['name']} {package['version']} — {license_value} — {source}\n"
            )
    seen = set()
    with open(args.licenses_output, "w", encoding="utf-8") as output:
        output.write("samdebug third-party Rust license and notice texts\n")
        output.write("==================================================\n\n")
        for package in packages:
            package_root = Path(package["manifest_path"]).parent
            candidates = sorted(
                path
                for path in package_root.iterdir()
                if path.is_file()
                and path.name.upper().startswith(("LICENSE", "COPYING", "NOTICE"))
            )
            for path in candidates:
                content = path.read_bytes()
                digest = hashlib.sha256(content).hexdigest()
                key = (package["name"], package["version"], digest)
                if key in seen:
                    continue
                seen.add(key)
                output.write(
                    f"--- {package['name']} {package['version']} / {path.name} "
                    f"(SHA-256 {digest}) ---\n"
                )
                output.write(content.decode("utf-8", errors="replace"))
                if not content.endswith(b"\n"):
                    output.write("\n")
                output.write("\n")


if __name__ == "__main__":
    main()
