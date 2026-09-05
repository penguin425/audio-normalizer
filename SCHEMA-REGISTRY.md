# JSON contract registry

[`schema/schema-registry-v1.json`](schema/schema-registry-v1.json) is the
source of truth for Forge's top-level JSON contracts. It registers every JSON
Schema and pinned data document shipped in crates and native release archives.
The registry also registers itself and its own schema.

Each entry records:

- the repository path, contract family, integer version, exact document ID,
  and the discriminator carried by instances (the document ID is the declared
  `$id` for JSON Schemas);
- whether a historical identifier must be preserved for compatibility;
- whether the document is a wire, durable, cache, report, evidence, or
  registry contract;
- its lifecycle, evolution policy, logical producers and consumers, and test
  owners;
- the next supported version when an older contract remains readable.

Published `$id` values are immutable. Fifteen existing schemas use historical
GitHub-hosted or `.schema.json`-suffixed identifiers; these are deliberately
marked `legacy-preserved` instead of being silently renamed.

## Adding or replacing a contract

1. Add a new versioned JSON document. Do not edit an already published schema
   incompatibly.
2. Add it to the registry with at least one producer, consumer, and validator.
3. If it replaces a version that remains readable, mark the old entry
   `supported-legacy` and set `successor_path`. Durable state needs a migration;
   caches may instead use explicit invalidation.
4. Add contract fixtures or tests and run:

   ```sh
   python3 tools/check-schema-registry.py
   python3 -m unittest tools/test_schema_registry.py
   cargo test --locked --test schema_registry
   ```

The checker rejects unregistered or stale files, duplicate IDs, version and
family mismatches, missing owners, broken successor chains, and non-local
`$ref` dependencies. The Rust test validates the registry against its schema,
compiles every registered schema without network access, and validates any
registered JSON samples.
