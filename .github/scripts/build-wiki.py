#!/usr/bin/env python3
"""Build the GitHub wiki pages from docs/.

docs/ stays the source (versioned with the code, reviewed in PRs); the wiki is a copy that
the Wiki workflow regenerates on every push to main. Usage:

    python build-wiki.py OUT_DIR --repo owner/name [--ref main]

OUT_DIR receives one flat .md file per page (the wiki has no folders), plus _Sidebar.md and
_Footer.md. Each page is named after its first heading ("# 3. Instalar o app" becomes
Instalar-o-app.md), and that heading is dropped because the wiki already shows the page
name as the title. Links between published docs become wiki links; links to anything else
in the repository become links to the file on GitHub. A relative link to a missing file
fails the build.
"""

import argparse
import re
import sys
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[2]

# What goes to the wiki, in sidebar order: (source under the repo root, sidebar section).
# The guide's index becomes Home. PLAN.md and TESTING.md are for development, not readers.
PAGES = [
    ("docs/guia/README.md", None),
    *[(f"docs/guia/{p.name}", "Guia") for p in sorted((ROOT / "docs/guia").glob("[0-9][0-9]-*.md"))],
    ("docs/SERVER.md", "Referência"),
    ("docs/RELEASE.md", "Referência"),
]

LINK = re.compile(r"\]\(([^)\s]+)\)")
FENCE = re.compile(r"^\s*(```|~~~)")


def title_of(text: str, source: str) -> str:
    m = re.match(r"# (.+)\n", text)
    if not m:
        sys.exit(f"{source}: must start with a '# ' heading to name the wiki page after")
    return m.group(1).strip()


def page_name(title: str) -> str:
    """'3. Instalar o app' -> 'Instalar-o-app'; 'Servidor privado (`rdesk-server`)' ->
    'Servidor-privado-rdesk-server'. Accents stay: the wiki handles them."""
    title = re.sub(r"^\d+\.\s*", "", title)
    words = re.findall(r"[\w-]+", title)
    return "-".join(words)


def split_target(target: str) -> tuple[str, str]:
    path, _, anchor = target.partition("#")
    return path, ("#" + anchor if anchor else "")


def rewrite_links(text: str, source: str, names: dict[str, str], repo: str, ref: str) -> str:
    base = PurePosixPath(source).parent

    def fix(m: re.Match) -> str:
        target = m.group(1)
        if target.startswith("#") or re.match(r"^[a-z][a-z0-9+.-]*:", target, re.I):
            return m.group(0)  # same-page anchor, http(s), mailto…
        path, anchor = split_target(target)
        resolved = normalize(base / path)
        if not (ROOT / resolved).exists():
            sys.exit(f"{source}: link to {target} points to {resolved}, which does not exist")
        if resolved in names:
            return f"]({names[resolved]}{anchor})"
        kind = "tree" if (ROOT / resolved).is_dir() else "blob"
        return f"](https://github.com/{repo}/{kind}/{ref}/{resolved}{anchor})"

    out, in_code = [], False
    for line in text.splitlines(keepends=True):
        if FENCE.match(line):
            in_code = not in_code
        out.append(line if in_code else LINK.sub(fix, line))
    return "".join(out)


def normalize(path: PurePosixPath) -> str:
    parts: list[str] = []
    for part in path.parts:
        if part == "..":
            if not parts:
                sys.exit(f"link leaves the repository: {path}")
            parts.pop()
        elif part != ".":
            parts.append(part)
    return "/".join(parts)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("out", type=Path)
    ap.add_argument("--repo", required=True, help="owner/name, for links to repository files")
    ap.add_argument("--ref", default="main")
    args = ap.parse_args()

    texts = {src: (ROOT / src).read_text(encoding="utf-8") for src, _ in PAGES}
    titles = {src: title_of(texts[src], src) for src, _ in PAGES}
    names = {src: page_name(titles[src]) for src, _ in PAGES}
    names[PAGES[0][0]] = "Home"
    dupes = {n for n in names.values() if list(names.values()).count(n) > 1}
    if dupes:
        sys.exit(f"two docs would become the same wiki page: {', '.join(sorted(dupes))}")

    args.out.mkdir(parents=True, exist_ok=True)
    for src, _ in PAGES:
        body = re.sub(r"\A# .+\n+", "", texts[src])
        body = rewrite_links(body, src, names, args.repo, args.ref)
        (args.out / f"{names[src]}.md").write_text(body, encoding="utf-8", newline="\n")

    sidebar, section = [f"**[{titles[PAGES[0][0]]}](Home)**"], None
    for src, sec in PAGES[1:]:
        if sec != section:
            sidebar += ["", f"**{sec}**", ""]
            section = sec
        sidebar.append(f"- [{titles[src].replace('`', '')}]({names[src]})")
    (args.out / "_Sidebar.md").write_text("\n".join(sidebar).strip() + "\n", encoding="utf-8", newline="\n")

    (args.out / "_Footer.md").write_text(
        f"Esta wiki é gerada a partir da pasta [docs/](https://github.com/{args.repo}/tree/{args.ref}/docs) "
        "do repositório. Para corrigir algo, mude lá: edições feitas aqui são apagadas na "
        "próxima atualização.\n",
        encoding="utf-8",
        newline="\n",
    )
    print(f"{len(PAGES)} pages written to {args.out}")


if __name__ == "__main__":
    main()
