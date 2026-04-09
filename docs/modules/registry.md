# registry - Extension Registry and Installation Governance

## Overview

The `registry` crate discovers, downloads, verifies, installs, and upgrades Skill and Tool extensions. It does not execute extensions — its role is to turn an "external artifact" into a "governable artifact."

Core responsibilities:

- Maintain the registry index
- Download and verify extension artifacts (hash + optional signature)
- Record source, version, hash, signature, and trust level
- Install artifacts into a local directory for `tools` and `skills` to consume

## Design Decisions

### Governance entry point

Installation flow: catalog index → `ExtensionManifest` → download → hash/signature verify → install to `install_root` → consumed by tools and skills.

### Source of trust levels

`registry` maps external sources into governance fields: `source_url`, `artifact_hash`, `signature`, `trust_level`. `skills` and `tools` consume these fields directly rather than inferring trustworthiness on their own.

### Installation constraints

- Failed verification → artifact must not be written executably
- Install directory must be separate from user-authored workspace directories
- Upgrades preserve old version metadata for Trace replay and troubleshooting
- Successful installation does not imply auto-execution privileges

## Constraints

- Depends only on `core`
- Hash verification is mandatory; signature verification can be optional initially
- Registry download failures should not block already-installed extensions

## Collaboration

| Module | Role |
|--------|------|
| `skills` | Provides source, version, and trust level for installed skills |
| `tools` | Provides source, version, and artifact hash for installed tools |
| `trace` | Needs registry metadata for provenance during execution |
| `workspace` | Local workspace extensions and registry directories should be separate |
