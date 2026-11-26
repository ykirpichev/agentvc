#!/usr/bin/env python3
"""Validate the skills under skills/ against the Agent Skills spec and zip them.

    scripts/package-skills.py              # validate only (CI)
    scripts/package-skills.py --zip DIR    # also write DIR/<name>.zip per skill

Checks (https://agentskills.io/specification):
- SKILL.md starts with YAML frontmatter holding `name` and `description`;
- name: 1-64 chars, [a-z0-9-], no leading/trailing/double hyphen, equal to
  the directory name;
- description: 1-1024 chars; compatibility: at most 500 chars;
- metadata, if present, maps strings to strings;
- SKILL.md is under 500 lines, and every relative link in it resolves;
- the package holds only Markdown and is not executable (marketplaces scan
  for scripts; these skills tell the agent which commands to run instead).

Standard library only; the frontmatter subset used here is parsed by hand.
"""

import argparse
import os
import re
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SKILLS = ROOT / "skills"
NAME_RE = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*$")
KNOWN = {"name", "description", "license", "compatibility", "metadata", "allowed-tools"}
# Fixed timestamp so the zips are byte-for-byte reproducible.
ZIP_TIME = (2026, 1, 1, 0, 0, 0)


def unquote(v):
    v = v.strip()
    if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
        return v[1:-1]
    return v


def frontmatter(text):
    lines = text.split("\n")
    if not lines or lines[0].strip() != "---":
        raise ValueError("SKILL.md must start with '---' frontmatter")
    try:
        end = lines.index("---", 1)
    except ValueError:
        raise ValueError("frontmatter is not closed with '---'")
    fields, current = {}, None
    for line in lines[1:end]:
        if not line.strip():
            continue
        if line.startswith((" ", "\t")):
            if current != "metadata":
                raise ValueError(f"unexpected indented line: {line!r}")
            k, sep, v = line.strip().partition(":")
            if not sep:
                raise ValueError(f"bad metadata line: {line!r}")
            fields["metadata"][k.strip()] = unquote(v)
            continue
        k, sep, v = line.partition(":")
        if not sep:
            raise ValueError(f"bad frontmatter line: {line!r}")
        current = k.strip()
        fields[current] = {} if current == "metadata" and not v.strip() else unquote(v)
    return fields, "\n".join(lines[end + 1 :])


def check(skill_dir):
    errors = []
    md = skill_dir / "SKILL.md"
    if not md.is_file():
        return ["missing SKILL.md"]
    text = md.read_text(encoding="utf-8")
    try:
        fm, body = frontmatter(text)
    except ValueError as e:
        return [str(e)]

    for k in fm:
        if k not in KNOWN:
            errors.append(f"unknown frontmatter field {k!r}")
    name = fm.get("name", "")
    if not (1 <= len(name) <= 64 and NAME_RE.match(name)):
        errors.append(f"name {name!r} must be 1-64 chars of a-z, 0-9 and single hyphens")
    if name != skill_dir.name:
        errors.append(f"name {name!r} must equal the directory name {skill_dir.name!r}")
    desc = fm.get("description", "")
    if not 1 <= len(desc) <= 1024:
        errors.append(f"description must be 1-1024 chars (is {len(desc)})")
    if "compatibility" in fm and not 1 <= len(fm["compatibility"]) <= 500:
        errors.append(f"compatibility must be 1-500 chars (is {len(fm['compatibility'])})")
    if "metadata" in fm and not isinstance(fm["metadata"], dict):
        errors.append("metadata must be a map of strings")
    if text.count("\n") >= 500:
        errors.append("SKILL.md must be under 500 lines")

    for target in re.findall(r"\]\(([^)#\s]+)(?:#[^)]*)?\)", body):
        if re.match(r"^[a-z]+:", target):
            continue
        if not (skill_dir / target).is_file():
            errors.append(f"broken link {target!r}")

    for p in sorted(skill_dir.rglob("*")):
        if p.is_file():
            rel = p.relative_to(skill_dir)
            if p.suffix != ".md":
                errors.append(f"{rel}: only Markdown files are allowed in a skill package")
            if os.access(p, os.X_OK):
                errors.append(f"{rel}: must not be executable")
    return errors


def write_zip(skill_dir, out_dir):
    out = out_dir / f"{skill_dir.name}.zip"
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        for p in sorted(skill_dir.rglob("*")):
            if p.is_file():
                arc = f"{skill_dir.name}/{p.relative_to(skill_dir).as_posix()}"
                info = zipfile.ZipInfo(arc, ZIP_TIME)
                info.external_attr = 0o644 << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                z.writestr(info, p.read_bytes())
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--zip", metavar="DIR", type=Path, help="write one zip per skill here")
    args = ap.parse_args()

    dirs = sorted(d for d in SKILLS.iterdir() if d.is_dir())
    if not dirs:
        print(f"no skills under {SKILLS}", file=sys.stderr)
        return 1
    failed = False
    for d in dirs:
        errors = check(d)
        if errors:
            failed = True
            for e in errors:
                print(f"skills/{d.name}: {e}", file=sys.stderr)
        else:
            print(f"ok  skills/{d.name}")
    if failed:
        return 1
    if args.zip:
        args.zip.mkdir(parents=True, exist_ok=True)
        for d in dirs:
            print(f"zip {write_zip(d, args.zip)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
