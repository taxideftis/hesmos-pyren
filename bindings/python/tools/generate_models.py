#!/usr/bin/env python3
"""Generate bindings/python/hesmos/models.py from the api-contracts machine oracle.

Source of truth: .agent-team/04-architecture/api-contracts.md ```yaml ``contract:`` blocks
(TYPE-1..7). api-contracts §0.1 sanctions generating pydantic mirrors from those blocks and
data-model-erd §5 forbids manual edits to models.py — this script is the only write path.
Rerun it whenever the contract doc changes (contract-first rule: the doc is edited before code).

Parsing scope (deliberate): the Rust-lite subset used by the contract document — tuple
newtypes, flat field structs, fieldless and payload enums. The contract doc is frozen at the
baseline (project-context §0.2), so the parser tracks the document, not arbitrary Rust.

Types referenced but not defined in the contracts (e.g. DoneCriteria, ArtifactRef) are
emitted as opaque `Any` mirrors — the generator never invents field shapes.

Wire convention (FFI marshal contract, owned by hesmos-ffi::marshal):
- Structs: JSON objects with the Rust field names.
- Payload enums: internally tagged {"kind": "<Variant>", ...payload fields}.
- Fieldless enums: plain strings.

Usage: python3 bindings/python/tools/generate_models.py
Output is deterministic: identical contract input yields a byte-identical models.py
(tests/python/test_models.py enforces this, so hand edits cannot hide).
"""

from __future__ import annotations

import re
from pathlib import Path

import yaml

REPO = Path(__file__).resolve().parents[3]
CONTRACTS = REPO / ".agent-team" / "04-architecture" / "api-contracts.md"
OUT = REPO / "bindings" / "python" / "hesmos" / "models.py"

# ---------------------------------------------------------------------------
# Contract-prose-fixed shapes. Each entry is stated in the contract text (not
# invented): keep the citation in the comment when editing.
# ---------------------------------------------------------------------------
PROSE_ALIASES = {
    # TYPE-6 AgentProfile comment: model "glm-5.3-flash" (SS-17) — model refs are strings.
    "ModelRef": "str",
    # TYPE-4 field comment: "0.0~1.0".
    "Confidence": "Annotated[float, Field(ge=0.0, le=1.0)]",
    # Identity-ish names referenced by the contracts without a defining block —
    # string mirrors (consistent newtype representation decision, WP-P0c D4).
    "SchemaId": "str",
    "TeamId": "str",
    "AgentRole": "str",
    "ToolName": "str",
    # TYPE-3 attrs comment: "stop_kind: done|retry|escalate|reject"
    "StopKind": 'Literal["done", "retry", "escalate", "reject"]',
    # TYPE-3 attrs comment: "level: warn|suspend"
    "BudgetLevel": 'Literal["warn", "suspend"]',
    # TYPE-2 Envelope comment: "Task | Result | Control | Knowledge" — no pub enum in the doc.
    "EnvelopeKind": 'Literal["Task", "Result", "Control", "Knowledge"]',
    # TYPE-3 TraceEvent: attrs is the kind-keyed property map (attrs_required/attrs_optional).
    "EventAttrs": "dict[str, Any]",
    # TYPE-2 field taint: comment union emitted as TaintClean/TaintTainted classes below.
    "Taint": 'Annotated[Union[TaintClean, TaintTainted], Field(discriminator="kind")]',
    # TYPE-6 Plan comment: "Sequential | Parallel | Swarm | Graph (표 9)".
    "PatternKind": 'Literal["Sequential", "Parallel", "Swarm", "Graph"]',
    # TYPE-7 HesmosError comment: "Reason | Compile | Platform | Usage | Ffi" (exceptions §1).
    "ErrorClass": 'Literal["Reason", "Compile", "Platform", "Usage", "Ffi"]',
}
# Structured types cited by the contracts with no field definitions — opaque mirrors.
OPAQUE_TYPES = {
    "DoneCriteria",
    "ArtifactRef",
    "FailedApproach",
    "Assumption",
    "PermissionCap",
    "KnowledgeEntry",
    "PolicyOverrides",
    "KnowledgeSpec",
    "BudgetSpec",
    "Edge",
    "GateSpec",
    "ResourceLimits",
    "NetworkScope",
    "TaintSource",
    "LeaseRegistry",
}
# Structs whose shape the contract states only in a comment (no `pub struct` block).
# Each entry: [(rust_field, rust_type)] — emitted as frozen BaseModels in the header.
PROSE_STRUCTS = {
    # TYPE-2 Envelope field comment: "{ schema_id: SchemaId, json: serde_json::Value }"
    "Payload": [("schema_id", "SchemaId"), ("json", "serde_json::Value")],
}

# Rust primitive -> Python annotation. Collections are handled by map_type.
PRIMITIVES = {
    "String": "str",
    "bool": "bool",
    "u8": "int",
    "u16": "int",
    "u32": "int",
    "u64": "int",
    "usize": "int",
    "i32": "int",
    "i64": "int",
    "f32": "float",
    "f64": "float",
    "serde_json::Value": "Any",
    "Any": "Any",
    # PY-3/PY-4 prose types are written Python-style ("str") — accepted alongside the
    # Rust spellings so prose registrations need no translation.
    "str": "str",
}
# Newtypes from TYPE-1. SSOT is the core ids.rs re-export list (P0a relay item 4):
# ULID newtypes (SessionId/RunId/EnvelopeId/CorrelationId) serialize as the canonical
# 26-char Crockford string and must validate as such — the leading char is limited to
# 0-7 because a ULID's first char carries the 48-bit timestamp's top bits (a higher
# char overflows u128 and core's from_string rejects it). String newtypes
# (NodeId/SchemaId/AgentRole/ToolName/TeamId/ModelRef) are unconstrained str on the
# wire (ids.rs string_id! macro — no format invariant).
CROCKFORD_ULID = r"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"  # Crockford: I/L/O/U never appear
NEWTYPE_ALIASES = {
    "SessionId": ("str", CROCKFORD_ULID),
    "RunId": ("str", CROCKFORD_ULID),
    "CorrelationId": ("str", CROCKFORD_ULID),
    "EnvelopeId": ("str", CROCKFORD_ULID),
    "NodeId": ("str", None),  # string_id! newtype — unconstrained
    "Sha256Hex": ("str", r"^[0-9a-f]{64}$"),
    "CommitSeq": ("int", None),
    "WaveIndex": ("int", None),
}
# Python keywords / shadowing names used as Rust field names -> rename + alias.
KEYWORD_FIELDS = {"from": "from_", "class": "class_", "json": "json_"}


def extract_contract_blocks(text: str) -> list[dict]:
    """Return parsed `contract:` YAML blocks in document order."""
    blocks = []
    fence = re.compile(r"```yaml\n(.*?)```", re.DOTALL)
    for match in fence.finditer(text):
        parsed = yaml.safe_load(match.group(1))
        if isinstance(parsed, dict) and "contract" in parsed:
            blocks.append(parsed["contract"])
    return blocks


def strip_comments(rust_src: str) -> str:
    return re.sub(r"//[^\n]*", "", rust_src)


def split_top_level(items: str) -> list[str]:
    """Split on commas that are not nested inside <> or {}."""
    out, depth, cur = [], 0, []
    for ch in items:
        if ch in "<{(":
            depth += 1
        elif ch in ">})":
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    if "".join(cur).strip():
        out.append("".join(cur))
    return [s.strip() for s in out if s.strip()]


def outer_type(t: str) -> str:
    """Inner type of a single-argument generic like Option<T> / Vec<T>."""
    return t[t.index("<") + 1 : t.rindex(">")].strip()


# Struct/enum names defined by the contracts themselves — populated in generate()
# before emission so cross-references (TraceEvent.kind -> EventKind) resolve.
KNOWN_NAMES: set[str] = set()


def map_type(t: str) -> str:
    t = t.strip()
    if t in PROSE_ALIASES:
        return PROSE_ALIASES[t]
    if t in KNOWN_NAMES:
        return t
    if t in PROSE_STRUCTS:
        return t
    if t in OPAQUE_TYPES:
        return "Any"
    if t in NEWTYPE_ALIASES:
        # Return the alias NAME, not its base: fields must reference the module-level
        # Annotated alias so constraints (Crockford ULID, Sha256Hex) actually enforce
        # inside every model, not only in standalone TypeAdapter use.
        return t
    if t in PRIMITIVES:
        return PRIMITIVES[t]
    if t.startswith("Option<"):
        return f"{map_type(outer_type(t))} | None"
    if t.startswith("(") and t.endswith(")"):
        # Rust tuple type, e.g. Option<(SessionId, CommitSeq)> for SessionHandle.fork_of.
        parts = split_top_level(t[1:-1])
        return "tuple[" + ", ".join(map_type(p) for p in parts) + "]"
    if t.startswith("Vec<"):
        return f"list[{map_type(outer_type(t))}]"
    if t.startswith("BTreeSet<"):
        # BTreeSet serializes as a sorted JSON array — list mirrors that wire shape.
        return f"list[{map_type(outer_type(t))}]"
    if t.startswith("BTreeMap<"):
        inner = t[t.index("<") + 1 : t.rindex(">")]
        key, _, value = inner.partition(",")
        # Contract maps are keyed by string-ish roles (AgentRole) — str keys on the wire.
        return f"dict[str, {map_type(value)}]"
    raise ValueError(f"Unmapped contract type: {t!r} — extend PROSE_ALIASES/OPAQUE_TYPES, never models.py")


def parse_structs_and_enums(rust_src: str) -> tuple[list[dict], list[dict]]:
    """Extract struct and enum declarations from one contract rust block."""
    src = strip_comments(rust_src)
    structs, enums = [], []
    for m in re.finditer(r"pub struct (\w+)\s*(\([^)]*\))?\s*(\{[^}]*\})?", src):
        name, tuple_inner, body = m.group(1), m.group(2), m.group(3)
        if tuple_inner:  # newtype: pub struct X(Inner);
            structs.append({"name": name, "newtype": tuple_inner.strip("() ").strip(";"), "fields": []})
        elif body:
            fields = []
            for line in split_top_level(body.strip("{}\n")):
                fm = re.match(r"pub (\w+)\s*:\s*(.+)", line)
                if fm:
                    fields.append((fm.group(1), fm.group(2).strip()))
            structs.append({"name": name, "newtype": None, "fields": fields})
    # Enum body supports one level of nested braces (payload variants like Pass { score }).
    for m in re.finditer(r"pub enum (\w+)\s*(\{(?:[^{}]|\{[^{}]*\})*\})", src):
        name, body = m.group(1), m.group(2).strip("{}\n")
        variants = []
        for part in split_top_level(body):
            vm = re.match(r"(\w+)\s*(?:\{([^}]*)\})?\s*,?$", part)
            if vm:
                payload = []
                if vm.group(2):
                    for line in split_top_level(vm.group(2)):
                        fm = re.match(r"(\w+)\s*:\s*(.+)", line.strip())
                        if fm:
                            payload.append((fm.group(1), fm.group(2).strip()))
                        else:
                            # Doc shorthand like SchemaParse{location}: name only, type unstated.
                            payload.append((line.strip(), "Any"))
                variants.append((vm.group(1), payload))
        enums.append({"name": name, "variants": variants})
    return structs, enums


def emit_field(py_name: str, ann: str, rust_name: str) -> str:
    # Option fields default to None (serde treats missing Option as None on the wire).
    default = " = None" if ann.endswith("| None") else ""
    if py_name != rust_name:
        return f"    {py_name}: {ann} = Field(alias={rust_name!r}){default}"
    return f"    {py_name}: {ann}{default}"


def emit_struct(s: dict, lines: list[str]) -> None:
    if s["newtype"]:
        return  # newtypes are emitted as module-level aliases, not classes
    lines.append(f"class {s['name']}(BaseModel):")
    lines.append("    model_config = _FROZEN")
    if not s["fields"]:
        lines.append("    pass")
    for rust_name, rust_type in s["fields"]:
        py_name = KEYWORD_FIELDS.get(rust_name, rust_name)
        lines.append(emit_field(py_name, map_type(rust_type), rust_name))
    lines.append("")


def emit_enum(e: dict, lines: list[str]) -> None:
    name, variants = e["name"], e["variants"]
    if all(not payload for _, payload in variants):
        opts = ", ".join(f'"{v}"' for v, _ in variants)
        lines.append(f"{name} = Literal[{opts}]")
        lines.append("")
        return
    # Payload enum -> internally tagged union (wire: {"kind": "<Variant>", ...}).
    union_members = []
    for variant, payload in variants:
        cls = f"{name}{variant}"
        union_members.append(cls)
        lines.append(f"class {cls}(BaseModel):")
        lines.append("    model_config = _FROZEN")
        lines.append(f'    kind: Literal["{variant}"] = "{variant}"')
        for field, ftype in payload:
            py_field = KEYWORD_FIELDS.get(field, field)
            lines.append(emit_field(py_field, map_type(ftype), field))
        lines.append("")
    lines.append(f"{name} = Annotated[Union[{', '.join(union_members)}], Field(discriminator='kind')]")
    lines.append("")


def generate() -> str:
    text = CONTRACTS.read_text(encoding="utf-8")
    contracts = {c["id"]: c for c in extract_contract_blocks(text)}
    # PY-3/PY-4 wire types (LlmRequest/LlmReply/Message/ToolDef/ToolCall/ToolResult/
    # CacheBreakpoints) exist only as Python-style prose in the contracts — no Rust pub
    # struct blocks to parse. Registered here as prose structs so the generator stays
    # the single path to models.py; field lists mirror api-contracts PY-3 types/PY-4.
    # Message role set is the provider-neutral minimum; the P2c adapter maps it onto
    # the GLM/Anthropic wire format (prompt/ never knows provider specifics).
    PROSE_ALIASES["MessageRole"] = 'Literal["system", "user", "assistant", "tool"]'
    for prose_name, prose_fields in {
        "Message": [
            ("role", "MessageRole"),
            ("content", "str"),
            ("tool_call_id", "Option<str>"),
        ],
        "ToolDef": [
            ("name", "ToolName"),
            ("description", "str"),
            ("parameters_schema", "serde_json::Value"),
        ],
        "ToolCall": [("id", "str"), ("name", "ToolName"), ("arguments", "serde_json::Value")],
        # SS-20 rule 1: external-doc results MUST carry taint explicitly — no default.
        "ToolResult": [("call_id", "str"), ("content", "serde_json::Value"), ("taint", "Taint")],
        # SS-18 rule 5 carrier: exclusive prefix lengths into LlmRequest.messages for
        # the stable/context/volatile layers. volatile == len(messages) (uncached tail).
        "CacheBreakpoints": [("stable", "usize"), ("context", "usize"), ("volatile", "usize")],
        "LlmRequest": [
            ("messages", "Vec<Message>"),
            ("temperature", "Option<f64>"),
            ("max_tokens", "Option<u64>"),
            ("tools_schema", "Vec<ToolDef>"),
            ("cache_breakpoints", "CacheBreakpoints"),
        ],
        "LlmReply": [
            ("content", "str"),
            ("tool_calls", "Vec<ToolCall>"),
            ("tokens_in", "u64"),
            ("tokens_out", "u64"),
            ("provider_meta", "EventAttrs"),
        ],
    }.items():
        PROSE_STRUCTS[prose_name] = prose_fields
        KNOWN_NAMES.add(prose_name)
    lines = [
        '# GENERATED by bindings/python/tools/generate_models.py — DO NOT EDIT.',
        "# Source: .agent-team/04-architecture/api-contracts.md contract: blocks (TYPE-1..7).",
        "# Rerun the generator when the contract changes; tests/python/test_models.py asserts",
        "# this file is byte-identical to a fresh generation (manual edits cannot hide).",
        "# Wire convention: payload enums are internally tagged {\"kind\": \"<Variant>\"};",
        "# fieldless enums are plain strings (see generator docstring).",
        "from __future__ import annotations",
        "",
        "from typing import Annotated, Any, Literal, Union",
        "",
        "from pydantic import BaseModel, ConfigDict, Field, StringConstraints",
        "",
        "# Frozen at the type level: Python cannot mutate boundary data (data-model-erd §5).",
        '# extra="forbid": fields beyond the contract are boundary violations (FFI-SCHEMA).',
        "_FROZEN = ConfigDict(frozen=True, extra='forbid', populate_by_name=True)",
        "",
    ]
    # TYPE-1 newtypes first (referenced by everything else).
    lines.append("# --- TYPE-1 identifiers (string/int mirrors; only Sha256Hex has a stated format invariant) ---")
    for name, (base, pattern) in NEWTYPE_ALIASES.items():
        if pattern:
            lines.append(f'{name} = Annotated[{base}, StringConstraints(pattern=r"{pattern}")]')
        else:
            lines.append(f"{name} = {base}")
    lines.append("")
    # Comment-stated shapes emitted before all contract structs (Payload precedes Envelope).
    # ReasonCode is defined by TYPE-7's reason_code_enum key, not a pub enum block.
    reason_codes = contracts["TYPE-7"].get("reason_code_enum") or []
    PROSE_ALIASES["ReasonCode"] = "Literal[" + ", ".join(f'"{c}"' for c in reason_codes) + "]"
    # TYPE-2 Taint: "Clean | Tainted { source: TaintSource }" — no pub enum in the doc;
    # TaintSource is emitted opaque (S5 taint transition rides this shape).
    for name, fields in PROSE_STRUCTS.items():
        lines.append(f"class {name}(BaseModel):")
        lines.append("    model_config = _FROZEN")
        for rust_name, rust_type in fields:
            py_name = KEYWORD_FIELDS.get(rust_name, rust_name)
            lines.append(emit_field(py_name, map_type(rust_type), rust_name))
        lines.append("")
    lines.extend(
        [
            "class TaintClean(BaseModel):",
            "    model_config = _FROZEN",
            '    kind: Literal["Clean"] = "Clean"',
            "",
            "",
            "class TaintTainted(BaseModel):",
            "    model_config = _FROZEN",
            '    kind: Literal["Tainted"] = "Tainted"',
            "    source: Any  # TaintSource has no defining block in api-contracts — opaque",
            "",
            "",
            'Taint = Annotated[Union[TaintClean, TaintTainted], Field(discriminator="kind")]',
            "",
        ]
    )
    # Pre-parse every contract to register all defined names before emission —
    # map_type then resolves cross-references like TraceEvent.kind -> EventKind.
    parsed: dict[str, tuple[list[dict], list[dict]]] = {}
    for type_id in sorted(contracts, key=lambda k: (len(k), k)):
        if type_id == "TYPE-1":
            continue
        structs, enums = parse_structs_and_enums(contracts[type_id].get("rust", ""))
        parsed[type_id] = (structs, enums)
        KNOWN_NAMES.update(s["name"] for s in structs)
        KNOWN_NAMES.update(e["name"] for e in enums)
    for type_id, (structs, enums) in parsed.items():
        c = contracts[type_id]
        lines.append(f"# --- {type_id} {c.get('name', '')} ---")
        for e in enums:
            emit_enum(e, lines)
        for s in structs:
            emit_struct(s, lines)
    # TYPE-3 attrs maps (kind -> required/optional attrs) as data constants.
    t3 = contracts["TYPE-3"]
    for key, const in (("attrs_required", "EVENT_ATTRS_REQUIRED"), ("attrs_optional", "EVENT_ATTRS_OPTIONAL")):
        lines.append(f"{const}: dict[str, dict[str, Any]] = {{")
        for kind, attrs in (t3.get(key) or {}).items():
            lines.append(f"    {json_key(kind)}: {json_list(attrs)},")
        lines.append("}")
        lines.append("")
    t4 = contracts["TYPE-4"]
    lines.append("# TYPE-4 field_absence_matrix — T4 reject oracle (api-contracts TYPE-4)")
    lines.append("FIELD_ABSENCE_MATRIX: dict[str, str] = {")
    for field, verdict in (t4.get("field_absence_matrix") or {}).items():
        lines.append(f"    {json_key(field)}: {json_str(verdict)},")
    lines.append("}")
    lines.append("")
    # REASON_CODES reuses the TYPE-7 reason_code_enum parsed above (the enum lives in
    # TYPE-7, not TYPE-4 — a second lookup here would silently emit an empty tuple).
    lines.append(f"REASON_CODES = {tuple_str(reason_codes)}")
    lines.append("")
    return "\n".join(lines) + "\n"


def json_key(s: str) -> str:
    return f'"{s}"'


def json_str(s: str) -> str:
    return '"' + str(s).replace('"', '\\"') + '"'


def json_list(items: list) -> str:
    return "[" + ", ".join(json_str(i) for i in items) + "]"


def tuple_str(items: list) -> str:
    return "(" + ", ".join(json_str(i) for i in items) + ")"


if __name__ == "__main__":
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(generate(), encoding="utf-8")
    print(f"wrote {OUT}")
