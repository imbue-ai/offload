# Architecture

## Design Principles

### Rust owns all logic; Python is a thin SDK wrapper

`scripts/modal_sandbox.py` exists solely because the Modal SDK is Python-only. It is a **thin, stateless wrapper** that translates CLI arguments into Modal API calls and returns results (image IDs, sandbox IDs) to stdout.

All caching, fallback, retry, and decision-making logic lives in Rust. Python must not accrete functionality beyond what is strictly required to call the Modal SDK. If a feature can be implemented in Rust, it must be.

### No Lock acquisitions or Atomics within a single thread

After creating an Arc, Mutex, RwLock, AtomicUsize or any other type with thread-safe interior mutability, you must prove that the contained type is actually in contention between multiple threads. Otherwise, you must find a different way to appease the borrow checker.

### Choose dependencies over custom implementation ALWAYS

If you're implementing something system-level, search far and wide in tokio for that functionality first. For algorithms and data structures, find popular crates on crates.io

### Framework, SandboxProvider, and Sandbox should be SIMPLE TRAITS

We expect people to implement these traits themselves. We want to make this as simple as possible. We do this by giving these traits small easy-to-understand interfaces

## Components

<!-- TODO: fill out broader architectural overview -->

## Versioning

A change is **breaking** if it would cause a previously correct `offload run` invocation — same CLI flags, same `offload.toml`, same test suite — to be rejected or to produce a different exit code. Everything else (new optional fields, new warnings, internal refactors, message changes) is not breaking.
