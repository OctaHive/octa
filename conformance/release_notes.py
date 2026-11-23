"""Select the tagged release's changelog entry for the GitHub release body."""

import argparse
import re
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--changelog", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    changelog = arguments.changelog.read_text(encoding="utf-8")
    sections = list(re.finditer(r"^## (.+)$", changelog, re.MULTILINE))
    for index, section in enumerate(sections):
        if section.group(1).startswith(arguments.version + " - "):
            end = sections[index + 1].start() if index + 1 < len(sections) else len(changelog)
            arguments.output.write_text(changelog[section.start():end].strip() + "\n", encoding="utf-8")
            return
    parser.error(f"no changelog entry for version {arguments.version}")


if __name__ == "__main__":
    main()
