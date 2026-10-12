# Resolver runtime boundary (retired)

This document described the measured resolver runtime: the Docker profile
`linux-amd64-v1` and the dedicated-server profiles `linux-dedicated-v1` and
`linux-dedicated-pg16-v1` (#608, #609), with their containment, process and
socket observation, executable identity and verified channels. #1636 removed
them ([DEC-1636.1](decisions/resolver.md#dec-1636-1)); the resolver that
remains is specified in
[SPEC §9.3.2](SPEC.md#932-engine-assisted-planning-accepted-not-implemented).

Decision records and pitfalls written before #1636 still cite this document
and its sections. Its text is in the git history of this file.
