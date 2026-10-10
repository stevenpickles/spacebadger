"""Adds the npm packages bundled into the web interface to the Rust notices.

    python3 scripts/release/web_notices.py RUST_HTML SOURCEMAP_DIR OUT_HTML

The packages are read from the sourcemaps of a build of the interface, so
only code that actually ships is listed. Each package's license file is
copied from node_modules.
"""

import glob
import html
import json
import pathlib
import re
import sys


def bundled_packages(map_dir):
    names = set()
    for path in glob.glob(f"{map_dir}/**/*.map", recursive=True):
        for source in json.loads(pathlib.Path(path).read_text(encoding="utf-8"))["sources"]:
            found = re.search(r"node_modules/((?:@[^/]+/)?[^/]+)", source)
            if found:
                names.add(found.group(1))
    return sorted(names)


def license_text(package_dir):
    files = sorted(
        p for p in package_dir.iterdir() if re.match(r"(?i)^(licen[cs]e|copying)", p.name) and p.is_file()
    )
    if not files:
        sys.exit(f"no license file in {package_dir}")
    return "\n\n".join(p.read_text(encoding="utf-8").strip() for p in files)


def main(rust_html, map_dir, out_html):
    page = pathlib.Path(rust_html).read_text(encoding="utf-8")
    overview = []
    sections = ["<h2>JavaScript packages</h2>"]
    for name in bundled_packages(map_dir):
        package_dir = pathlib.Path("node_modules", name)
        meta = json.loads((package_dir / "package.json").read_text(encoding="utf-8"))
        anchor = "npm-" + re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")
        url = f"https://www.npmjs.com/package/{name}"
        overview.append(f'  <li><a href="#{anchor}">{html.escape(name)}</a> ({html.escape(meta.get("license", "?"))})</li>')
        sections.append(
            f'<h3 id="{anchor}"><a href="{url}">{html.escape(name)} {html.escape(meta["version"])}</a>'
            f' ({html.escape(meta.get("license", "?"))})</h3>\n<pre>{html.escape(license_text(package_dir))}</pre>'
        )
    if len(sections) == 1:
        sys.exit(f"no bundled packages found in {map_dir}")
    for marker, content in (("<!-- web-overview -->", "\n".join(overview)), ("<!-- web-licenses -->", "\n".join(sections))):
        if page.count(marker) != 1:
            sys.exit(f"{rust_html} needs exactly one {marker}")
        page = page.replace(marker, content)
    pathlib.Path(out_html).write_text(page, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    main(*sys.argv[1:])
