"""Runfiles lookup support for Python tools executed by Rush."""

import os


def _manifest_entries(path):
    entries = {}
    try:
        with open(path, encoding="utf-8") as manifest:
            for line in manifest:
                line = line.rstrip("\n")
                if not line:
                    continue
                logical, separator, physical = line.partition(" ")
                entries[logical] = physical if separator else logical
    except OSError:
        return {}
    return entries


class _Runfiles:
    def __init__(self, env):
        self._manifest_path = env.get("RUNFILES_MANIFEST_FILE")
        self._manifest = (
            _manifest_entries(self._manifest_path) if self._manifest_path else {}
        )
        roots = []
        runfiles_dir = env.get("RUNFILES_DIR") or env.get("JAVA_RUNFILES")
        if runfiles_dir:
            roots.append(runfiles_dir)
        roots.extend(
            root
            for root in env.get("RUSH_RUNFILES_ROOTS", "").split(os.pathsep)
            if root
        )
        self._roots = list(dict.fromkeys(roots))

    def Rlocation(self, path, source_repo=None):
        del source_repo
        if not path:
            raise ValueError("path must not be empty")
        if os.path.isabs(path):
            return path
        normalized = os.path.normpath(path)
        if normalized != path or normalized.startswith(".."):
            raise ValueError("path is not normalized: " + path)

        exact = self._manifest.get(path)
        if exact is not None:
            return exact
        prefix = path
        while "/" in prefix:
            prefix = prefix.rsplit("/", 1)[0]
            mapped = self._manifest.get(prefix)
            if mapped is not None:
                return mapped + path[len(prefix) :]

        first_candidate = None
        for root in self._roots:
            candidate = os.path.join(root, path)
            if first_candidate is None:
                first_candidate = candidate
            if os.path.exists(candidate):
                return candidate
        return first_candidate

    def EnvVars(self):
        result = {}
        if self._manifest_path:
            result["RUNFILES_MANIFEST_FILE"] = self._manifest_path
        if self._roots:
            result["RUNFILES_DIR"] = self._roots[0]
            result["JAVA_RUNFILES"] = self._roots[0]
            result["RUSH_RUNFILES_ROOTS"] = os.pathsep.join(self._roots)
        return result

    def CurrentRepository(self, frame=1):
        del frame
        return ""


def Create(env=None):
    """Create a runfiles resolver from standard Bazel and Rush variables."""
    values = os.environ if env is None else env
    resolver = _Runfiles(values)
    if not resolver._manifest and not resolver._roots:
        return None
    return resolver
