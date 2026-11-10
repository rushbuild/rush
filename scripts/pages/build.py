#!/usr/bin/env python3
"""Stage the release page and its assets for GitHub Pages."""

import argparse
from html import escape, unescape
from pathlib import Path
import re
import shutil
from urllib.parse import quote, urlsplit


ROOT = Path(__file__).resolve().parents[2]
SOURCE_URL = 'https://github.com/rushbuild/rush/blob/main/'
ASSETS = ('images/rush.png',)
PAGES = ('index.html', 'buck2.html', 'docs/design/compiler-discovery.html')


def published_link(match):
    value = unescape(match.group(1))
    parsed = urlsplit(value)
    if parsed.scheme or parsed.netloc or not parsed.path:
        return match.group(0)
    path = Path(parsed.path)
    if path.is_absolute() or '..' in path.parts or not (ROOT / path).is_file():
        raise ValueError('invalid local page link: ' + value)
    if path.suffix == '.md' or parsed.path in {'MAINTAINERS', 'LICENSES/MIT'}:
        destination = SOURCE_URL + quote(parsed.path, safe='/')
        if parsed.fragment:
            destination += '#' + parsed.fragment
        return 'href="' + escape(destination, quote=True) + '"'
    if parsed.path not in ASSETS + PAGES:
        raise ValueError('undeclared page asset: ' + value)
    return match.group(0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / '_site')
    args = parser.parse_args()
    for asset in ASSETS + PAGES:
        if not (ROOT / asset).is_file() or (ROOT / asset).is_symlink():
            raise ValueError('missing regular page asset: ' + asset)
    pages = {
        name: re.sub(r'href="([^"]+)"', published_link,
                     (ROOT / name).read_text())
        for name in PAGES
    }
    # Refuse an existing destination rather than overwriting a working tree.
    args.output.mkdir(parents=True, exist_ok=False)
    for name, page in pages.items():
        destination = args.output / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(page)
    for asset in ASSETS:
        destination = args.output / asset
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / asset, destination)
    print('Staged ' + ', '.join(PAGES + ASSETS))


if __name__ == '__main__':
    main()
