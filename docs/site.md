# Release website

The public release page is served at
[rushbuild.github.io/rush](https://rushbuild.github.io/rush/).
Its sources are index.html, buck2.html and the existing PNG logo.

The Pages workflow stages only index.html, buck2.html and images/rush.png.
Keep this explicit allowlist in scripts/pages/build.py. Documentation
links are rewritten to GitHub's rendered source pages so Markdown filenames
do not become missing routes on the static site. The repository's Rust source,
local artifacts and editable image project are not included in the deployment.

Preview the staged site with Python, using a destination that does not exist:

```sh
python3 scripts/pages/build.py --output _site
python3 -m http.server --directory _site 8000
```

The builder refuses to overwrite an existing destination. Select a new output
directory when rebuilding a local preview. The generated _site directory is
ignored by Git.

Changes to the page, logo, builder or workflow on main trigger publication.
The workflow can also be dispatched manually. It checks out source without
retaining Git credentials, builds with read-only repository permissions, and
grants Pages deployment and OpenID Connect token rights only to the deployment
job. Only main may deploy. Review website and workflow changes through the
normal project rules; this workflow runs no model or engine build.
