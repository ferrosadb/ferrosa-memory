#!/usr/bin/env python3
"""
Ingest the ferrosa codebase dependency graph into ferrosa-memory.

Extracts crate dependencies, module structure, cross-module imports, and
public API surface from the ferrosa workspace, then inserts entities and
CO_OCCURS_WITH edges into the agent_memory keyspace.

Entity naming conventions:
  - crate:ferrosa-storage
  - mod:ferrosa-storage::engine
  - fn:ferrosa-storage::engine::compact
  - struct:ferrosa-common::Token
  - enum:ferrosa-index::IndexError
  - trait:ferrosa-sstable::ReadAt

Edge semantics (all stored as co_occurs_with):
  - crate depends_on crate   (strength=1.0)
  - crate contains module     (strength=0.9)
  - module uses module         (strength=0.7)
  - module contains item       (strength=0.8)
"""

import os
import re
import uuid
import datetime
from pathlib import Path

from cassandra.cluster import Cluster

# ── Configuration ─────────────────────────────────────────────────────────────

FERROSA_ROOT = Path(__file__).resolve().parents[2] / "ferrosa"
KEYSPACE = "agent_memory"
CQL_HOST = "127.0.0.1"
CQL_PORT = 19042
PROTOCOL_VERSION = 4

SESSION_ID = uuid.UUID("00000000-0000-0000-0000-000000000002")
TENANT_ID = uuid.UUID("00000000-0000-0000-0000-000000000000")

WORKSPACE_CRATES = [
    "ferrosa",
    "ferrosa-cluster",
    "ferrosa-common",
    "ferrosa-cql",
    "ferrosa-ctl",
    "ferrosa-graph",
    "ferrosa-index",
    "ferrosa-jepsen",
    "ferrosa-net",
    "ferrosa-schema",
    "ferrosa-sstable",
    "ferrosa-storage",
    "ferrosa-udf",
    "ferrosa-worker",
]

# ── Helpers ───────────────────────────────────────────────────────────────────

# Deterministic UUID from entity name (UUID5 with a fixed namespace).
# Re-runs produce the same UUIDs, making the script idempotent.
NAMESPACE_UUID = uuid.UUID("a1b2c3d4-e5f6-7890-abcd-ef1234567890")


def entity_uuid(name: str) -> uuid.UUID:
    """Deterministic UUID for an entity name."""
    return uuid.uuid5(NAMESPACE_UUID, name)


def parse_workspace_deps(cargo_toml_path: Path) -> list[str]:
    """Extract workspace crate dependencies from a Cargo.toml [dependencies] section."""
    deps = []
    if not cargo_toml_path.exists():
        return deps
    content = cargo_toml_path.read_text()
    # Match: ferrosa-common = { path = "../ferrosa-common" }
    for m in re.finditer(r"^(ferrosa[\w-]*)\s*=\s*\{[^}]*path\s*=", content, re.MULTILINE):
        dep_name = m.group(1)
        if dep_name in WORKSPACE_CRATES:
            deps.append(dep_name)
    return deps


def parse_mod_declarations(file_path: Path) -> list[str]:
    """Extract pub mod and mod declarations from a Rust source file."""
    mods = []
    if not file_path.exists():
        return mods
    content = file_path.read_text()
    for m in re.finditer(r"^\s*pub(?:\(crate\))?\s+mod\s+(\w+)\s*;", content, re.MULTILINE):
        mods.append(m.group(1))
    # Also match private mod (common in main.rs binaries)
    for m in re.finditer(r"^\s*mod\s+(\w+)\s*;", content, re.MULTILINE):
        mod_name = m.group(1)
        if mod_name not in mods:
            mods.append(mod_name)
    return mods


def parse_use_statements(file_path: Path) -> tuple[list[str], list[str]]:
    """
    Extract use statements from a Rust file.

    Returns:
        (internal_mods, cross_crate_idents)
        - internal_mods: first module segment from `use crate::...` lines
        - cross_crate_idents: Rust crate identifiers from `use ferrosa_*::...`
    """
    internal = []
    cross = []
    if not file_path.exists():
        return internal, cross
    content = file_path.read_text()

    for m in re.finditer(r"use\s+crate::(\w+)", content):
        internal.append(m.group(1))

    for m in re.finditer(r"use\s+(ferrosa_\w+)", content):
        cross.append(m.group(1))

    return list(set(internal)), list(set(cross))


def parse_public_items(file_path: Path) -> list[tuple[str, str]]:
    """
    Extract public item declarations from a Rust file.

    Returns list of (kind, name) tuples.
    """
    items = []
    if not file_path.exists():
        return items
    content = file_path.read_text()

    patterns = [
        (r"pub\s+(?:async\s+)?fn\s+(\w+)", "fn"),
        (r"pub\s+struct\s+(\w+)", "struct"),
        (r"pub\s+enum\s+(\w+)", "enum"),
        (r"pub\s+trait\s+(\w+)", "trait"),
    ]
    for pattern, kind in patterns:
        for m in re.finditer(pattern, content):
            name = m.group(1)
            if not name.startswith("test_"):
                items.append((kind, name))
    return items


def resolve_module_path(crate_name: str, rs_file: Path, crate_src: Path) -> str:
    """
    Resolve a .rs file path to a fully-qualified module path.

    src/engine.rs           -> ferrosa-storage::engine
    src/memtable/mod.rs     -> ferrosa-storage::memtable
    src/memtable/sharded.rs -> ferrosa-storage::memtable::sharded
    src/lib.rs              -> ferrosa-storage  (crate root)
    """
    rel = rs_file.relative_to(crate_src)
    parts = list(rel.parts)

    # Strip .rs extension
    if parts[-1].endswith(".rs"):
        parts[-1] = parts[-1][:-3]

    # mod.rs -> parent directory is the module
    if parts[-1] == "mod":
        parts = parts[:-1]

    # lib.rs / main.rs -> crate root
    if len(parts) == 1 and parts[0] in ("lib", "main"):
        return crate_name

    if parts and parts[0] in ("lib", "main"):
        parts = parts[1:]

    if not parts:
        return crate_name

    return f"{crate_name}::{'::'.join(parts)}"


# ── Extraction ────────────────────────────────────────────────────────────────

def extract_graph():
    """
    Walk the ferrosa workspace and build the dependency graph.

    Returns:
        entities: dict[str, dict]
        edges: list[tuple[str, str, float]]
    """
    entities: dict[str, dict] = {}
    edges: list[tuple[str, str, float]] = []

    def add_entity(name: str, entity_type: str, context: str = ""):
        if name not in entities:
            entities[name] = {
                "entity_type": entity_type,
                "entity_name": name,
                "context_snippet": context[:500] if context else "",
            }

    def add_edge(a: str, b: str, strength: float):
        edges.append((a, b, strength))

    for crate_name in WORKSPACE_CRATES:
        crate_dir = FERROSA_ROOT / crate_name
        if not crate_dir.exists():
            print(f"  SKIP {crate_name} (not found)")
            continue

        crate_entity = f"crate:{crate_name}"
        cargo_path = crate_dir / "Cargo.toml"

        # Read crate description for context
        cargo_desc = ""
        if cargo_path.exists():
            content = cargo_path.read_text()
            desc_match = re.search(r'description\s*=\s*"([^"]*)"', content)
            if desc_match:
                cargo_desc = desc_match.group(1)

        add_entity(
            crate_entity,
            "concept",
            f"Rust crate: {cargo_desc}" if cargo_desc else "Rust crate in ferrosa workspace",
        )

        # ── 1. Crate-level dependencies ──────────────────────────────────
        workspace_deps = parse_workspace_deps(cargo_path)
        for dep in workspace_deps:
            dep_entity = f"crate:{dep}"
            add_entity(dep_entity, "concept", "Rust crate in ferrosa workspace")
            add_edge(crate_entity, dep_entity, 1.0)
        print(f"  {crate_name}: {len(workspace_deps)} crate deps")

        # ── 2. Module structure ──────────────────────────────────────────
        src_dir = crate_dir / "src"
        entry_file = src_dir / "lib.rs"
        if not entry_file.exists():
            entry_file = src_dir / "main.rs"

        top_mods = parse_mod_declarations(entry_file)
        for mod_name in top_mods:
            mod_entity = f"mod:{crate_name}::{mod_name}"
            add_entity(mod_entity, "concept", f"Module {mod_name} in {crate_name}")
            add_edge(crate_entity, mod_entity, 0.9)
        print(f"  {crate_name}: {len(top_mods)} top-level modules")

        # ── 3. Per-file analysis ─────────────────────────────────────────
        rs_files = sorted(src_dir.rglob("*.rs")) if src_dir.exists() else []
        file_count = 0
        item_count = 0

        for rs_file in rs_files:
            mod_path = resolve_module_path(crate_name, rs_file, src_dir)

            # Register sub-module entity
            if mod_path != crate_name:
                mod_entity_name = f"mod:{mod_path}"
                add_entity(mod_entity_name, "concept", f"Module {mod_path}")

                # Nested module parent-contains-child edge
                parts = mod_path.split("::")
                if len(parts) > 2:
                    parent_path = "::".join(parts[:-1])
                    parent_entity = f"mod:{parent_path}"
                    add_entity(parent_entity, "concept", f"Module {parent_path}")
                    add_edge(parent_entity, mod_entity_name, 0.9)

            # Use statements
            internal_uses, cross_uses = parse_use_statements(rs_file)
            source_entity = f"mod:{mod_path}" if mod_path != crate_name else crate_entity

            for used_mod in internal_uses:
                target = f"mod:{crate_name}::{used_mod}"
                add_entity(target, "concept", f"Module {crate_name}::{used_mod}")
                add_edge(source_entity, target, 0.7)

            for used_ident in cross_uses:
                # ferrosa_common -> ferrosa-common
                used_crate = used_ident.replace("_", "-")
                target = f"crate:{used_crate}"
                add_entity(target, "concept", "Rust crate in ferrosa workspace")
                add_edge(source_entity, target, 0.7)

            # Public items
            items = parse_public_items(rs_file)
            for kind, item_name in items:
                item_entity = f"{kind}:{mod_path}::{item_name}"
                add_entity(item_entity, "concept", f"pub {kind} {item_name} in {mod_path}")
                container = f"mod:{mod_path}" if mod_path != crate_name else crate_entity
                add_edge(container, item_entity, 0.8)
                item_count += 1

            file_count += 1

        print(f"  {crate_name}: {file_count} files, {item_count} public items")

    return entities, edges


def deduplicate_edges(
    edges: list[tuple[str, str, float]],
) -> list[tuple[str, str, float]]:
    """Keep only the highest-strength edge for each (a, b) pair."""
    best: dict[tuple[str, str], float] = {}
    for a, b, s in edges:
        key = (a, b)
        if key not in best or best[key] < s:
            best[key] = s
    return [(a, b, s) for (a, b), s in best.items()]


# ── Ingestion ─────────────────────────────────────────────────────────────────

def ingest(entities: dict, edges: list):
    """Insert entities and edges into ferrosa-memory via the Cassandra driver."""
    cluster = Cluster(
        contact_points=[CQL_HOST],
        port=CQL_PORT,
        protocol_version=PROTOCOL_VERSION,
    )
    session = cluster.connect(KEYSPACE)
    print(f"  Connected to {CQL_HOST}:{CQL_PORT}/{KEYSPACE}")

    now = datetime.datetime.now(datetime.timezone.utc)

    # Prepared statements
    entity_stmt = session.prepare(
        "INSERT INTO entity_store "
        "(tenant_id, entity_id, session_id, entity_name, entity_type, "
        "context_snippet, confidence, created_at) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )

    edge_stmt = session.prepare(
        "INSERT INTO co_occurs_with "
        "(entity_a, entity_b, session_id, tenant_id, created_at, "
        "strength, last_reinforced) "
        "VALUES (?, ?, ?, ?, ?, ?, ?)"
    )

    # ── Insert entities ──────────────────────────────────────────────────
    entity_id_map: dict[str, uuid.UUID] = {}
    count = 0
    for name, info in entities.items():
        eid = entity_uuid(name)
        entity_id_map[name] = eid
        session.execute(
            entity_stmt,
            (
                TENANT_ID,
                eid,
                SESSION_ID,
                info["entity_name"],
                info["entity_type"],
                info["context_snippet"],
                1.0,  # confidence
                now,
            ),
        )
        count += 1
        if count % 100 == 0:
            print(f"  entities: {count}/{len(entities)}")

    print(f"  entities: {count}/{len(entities)} (done)")

    # ── Insert edges ─────────────────────────────────────────────────────
    edge_count = 0
    skipped = 0
    for a_name, b_name, strength in edges:
        a_id = entity_id_map.get(a_name)
        b_id = entity_id_map.get(b_name)
        if a_id is None or b_id is None:
            skipped += 1
            continue
        session.execute(
            edge_stmt,
            (
                a_id,
                b_id,
                SESSION_ID,
                TENANT_ID,
                now,
                strength,
                now,
            ),
        )
        edge_count += 1
        if edge_count % 200 == 0:
            print(f"  edges: {edge_count}/{len(edges)}")

    print(f"  edges: {edge_count}/{len(edges)} (done, {skipped} skipped)")

    cluster.shutdown()
    return count, edge_count


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    print("=" * 60)
    print("Ferrosa Dependency Graph -> ferrosa-memory ingestion")
    print("=" * 60)
    print()

    # ── Phase 1: Extract ─────────────────────────────────────────────────
    print("[1/3] Extracting dependency graph from ferrosa codebase...")
    print(f"  Root: {FERROSA_ROOT}")
    print()
    entities, edges = extract_graph()
    edges = deduplicate_edges(edges)
    print()
    print(f"  Totals: {len(entities)} entities, {len(edges)} edges")
    print()

    # Breakdown by prefix
    prefix_counts: dict[str, int] = {}
    for name in entities:
        prefix = name.split(":")[0]
        prefix_counts[prefix] = prefix_counts.get(prefix, 0) + 1
    print("  Entity breakdown:")
    for prefix, count in sorted(prefix_counts.items()):
        print(f"    {prefix:8s} {count}")
    print()

    # Crate dependency summary
    print("  Crate-level dependencies:")
    for a, b, s in sorted(edges):
        if a.startswith("crate:") and b.startswith("crate:") and s == 1.0:
            print(f"    {a} -> {b}")
    print()

    # ── Phase 2: Ingest ──────────────────────────────────────────────────
    print("[2/3] Inserting into ferrosa-memory CQL backend...")
    entity_count, edge_count = ingest(entities, edges)
    print()

    # ── Phase 3: Summary ─────────────────────────────────────────────────
    print("[3/3] Summary")
    print(f"  Entities inserted: {entity_count}")
    print(f"  Edges inserted:    {edge_count}")
    print(f"  Session ID:        {SESSION_ID}")
    print(f"  Tenant ID:         {TENANT_ID}")
    print()
    print("Done. Query the graph with Cypher:")
    print()
    print("  -- All crate dependencies")
    print("  MATCH (a:Entity)-[:CO_OCCURS_WITH]->(b:Entity)")
    print("  WHERE a.entity_name STARTS WITH 'crate:'")
    print("    AND b.entity_name STARTS WITH 'crate:'")
    print("  RETURN a.entity_name, b.entity_name")
    print()
    print("  -- Modules in a crate")
    print("  MATCH (c:Entity)-[:CO_OCCURS_WITH]->(m:Entity)")
    print("  WHERE c.entity_name = 'crate:ferrosa-storage'")
    print("    AND m.entity_name STARTS WITH 'mod:'")
    print("  RETURN m.entity_name")
    print()
    print("  -- Public API of a module")
    print("  MATCH (m:Entity)-[:CO_OCCURS_WITH]->(item:Entity)")
    print("  WHERE m.entity_name = 'mod:ferrosa-storage::engine'")
    print("    AND (item.entity_name STARTS WITH 'fn:'")
    print("     OR  item.entity_name STARTS WITH 'struct:'")
    print("     OR  item.entity_name STARTS WITH 'enum:')")
    print("  RETURN item.entity_name")


if __name__ == "__main__":
    main()
