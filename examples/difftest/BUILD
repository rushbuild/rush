cc_library(name = "lib", hdrs = ["lib.h"])
cc_binary(name = "app", srcs = ["main.cc"], deps = [":lib"])
# Header-dependency regression gate. Run:
#   scripts/differential_correctness.sh $PWD //:app lib.h rush-out/app
# Currently FAILS the edit-reflected + incremental==clean checks (header edit not
# tracked in the compile action key). Will PASS once -MMD/-MF depfile
# post-validation is wired (the depfile PARSER already exists, unwired, in
# crates/rush-build/src/depfile.rs).
