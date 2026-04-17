# Architecture

## Design Principles

### Rust owns all logic; Python is a thin SDK wrapper

`scripts/modal_sandbox.py` exists solely because the Modal SDK is Python-only. It is a **thin, stateless wrapper** that translates CLI arguments into Modal API calls and returns results (image IDs, sandbox IDs) to stdout.

All caching, fallback, retry, and decision-making logic lives in Rust. Python must not accrete functionality beyond what is strictly required to call the Modal SDK. If a feature can be implemented in Rust, it must be.

## Components

<!-- TODO: fill out broader architectural overview -->

## Versioning

A change is **breaking** if it would cause a previously correct `offload run` invocation — same CLI flags, same `offload.toml`, same test suite — to be rejected or to produce a different exit code. Everything else (new optional fields, new warnings, internal refactors, message changes) is not breaking.
