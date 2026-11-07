# Test basic genrule functionality

genrule(
    name = "gen_version",
    outs = ["version.h"],
    cmd = "echo '#define VERSION \"1.0.0\"' > $@",
)

genrule(
    name = "gen_config",
    srcs = ["config.txt"],
    outs = ["config.h"],
    cmd = "cat $(SRCS) | sed 's/^/#define /' > $(OUTS)",
)

cc_library(
    name = "app_lib",
    srcs = ["app.cc"],
    hdrs = [":gen_version", ":gen_config"],
)
