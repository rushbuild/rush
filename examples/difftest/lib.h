#pragma once
#include <string>
// Inline definition lives in the HEADER so editing this file MUST recompile any
// translation unit that #includes it. Regression fixture for the header-dependency
// under-build bug: rush's CppCompile action key excludes source headers, so an
// edit here currently does NOT invalidate main.cc's cached object (stale build).
// Flips to correct once depfile post-validation is wired (roadmap P1; Codex 019ee906).
inline std::string msg() { return "Hello"; }
