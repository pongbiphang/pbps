FROM alpine@sha256:865b95f46d98cf867a156fe4a135ad3fe50d2056aa3f25ed31662dff6da4eb62
# Git records the user's intent; no Rust or database development/runtime tools.
RUN apk add --no-cache git && \
    for tool in cargo rustc psql sqlcmd odbcinst; do \
        if command -v "$tool"; then exit 1; fi; \
    done
ENV GIT_CONFIG_NOSYSTEM=1
