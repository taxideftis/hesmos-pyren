"""Generated mirror tests: determinism, frozenness, and contract constants (T6/AC2).

AC2: models.py must be a GENERATED mirror — the byte-identical regeneration check
makes hand edits impossible to hide.
"""

import sys
from pathlib import Path

import pydantic

REPO = Path(__file__).resolve().parents[2]
# The generator is a build tool, not an installed module — its repo path is added
# directly; hesmos itself comes from the installed package (see test_exceptions).
sys.path.insert(0, str(REPO / "bindings" / "python" / "tools"))

from generate_models import generate  # noqa: E402
from hesmos import models  # noqa: E402
from pydantic import TypeAdapter  # noqa: E402


def test_models_py_is_generated_and_fresh():
    # Regenerate in memory and require byte identity with the committed file.
    committed = (REPO / "bindings" / "python" / "hesmos" / "models.py").read_text(encoding="utf-8")
    assert committed == generate(), (
        "models.py is stale or hand-edited — rerun bindings/python/tools/generate_models.py"
    )


def test_frozen_mirror_rejects_mutation():
    handle = models.SessionHandle.model_validate(
        {
            "session_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "run_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "seed": 42,
            "plan_hash": "a" * 64,
            "budget": {"warn_pct": 80, "suspend_pct": 100, "agent_max_tokens": {}},
            "state": "Init",
        }
    )
    try:
        handle.seed = 7
    except pydantic.ValidationError:
        pass  # frozen: Python cannot mutate boundary data (data-model-erd §5)
    else:
        raise AssertionError("SessionHandle is mutable — frozen contract broken")


def test_sha256hex_rejects_non_lowercase_hex64():
    # TYPE-1 invariant: "Sha256Hex는 64자 소문자 hex 외 기각".
    # Enforcement point is pydantic validation (models/TypeAdapter) — calling the bare
    # Annotated alias bypasses constraints by typing semantics, which is exactly why
    # boundary values must flow through model_validate, never hand-built.
    from pydantic import TypeAdapter

    adapter = TypeAdapter(models.Sha256Hex)
    for bad in ("A" * 64, "a" * 63, "z" * 64):
        try:
            adapter.validate_python(bad)
        except pydantic.ValidationError:
            pass
        else:
            raise AssertionError(f"Sha256Hex accepted {bad!r}")


def test_ulid_newtypes_reject_non_canonical_forms():
    # P0a relay item 2 (SSOT: core ids.rs ulid_id!): ULID newtypes are the canonical
    # 26-char Crockford string. 'I'/'L'/'O'/'U' never appear; a first char above '7'
    # overflows the 48-bit timestamp head that core's from_string rejects; any other
    # length is not the canonical wire form.
    adapter = TypeAdapter(models.SessionId)
    assert adapter.validate_python("01ARZ3NDEKTSV4RRFFQ69G5FAV") == "01ARZ3NDEKTSV4RRFFQ69G5FAV"
    for bad in ("I1ARZ3NDEKTSV4RRFFQ69G5FAV", "L1ARZ3NDEKTSV4RRFFQ69G5FAV",
                "O1ARZ3NDEKTSV4RRFFQ69G5FAV", "U1ARZ3NDEKTSV4RRFFQ69G5FAV",
                "91ARZ3NDEKTSV4RRFFQ69G5FAV", "01ARZ3NDEKTSV4RRFFQ69G5FA"):
        try:
            adapter.validate_python(bad)
        except pydantic.ValidationError:
            pass
        else:
            raise AssertionError(f"ULID newtype accepted {bad!r}")


def test_envelope_rejects_extra_fields():
    # TYPE-2 invariant: the envelope has exactly its 7 fields — extras are FFI-SCHEMA.
    envelope = {
        "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "from": "a",
        "to": "b",
        "kind": "Task",
        "payload": {"schema_id": "s", "json": {}},
        "correlation_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "taint": {"kind": "Clean"},
        "role_and_content": {"role": "user"},  # forbidden legacy shape (SS-01 rule 2)
    }
    try:
        models.Envelope.model_validate(envelope)
    except pydantic.ValidationError:
        pass
    else:
        raise AssertionError("Envelope accepted an off-contract field")


def test_envelope_keyword_aliases_round_trip():
    envelope = models.Envelope.model_validate(
        {
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "from": "a",
            "to": "b",
            "kind": "Task",
            "payload": {"schema_id": "s", "json": {"k": 1}},
            "correlation_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "taint": {"kind": "Tainted", "source": "web.search"},
        }
    )
    assert envelope.from_ == "a"
    assert envelope.payload.json_ == {"k": 1}
    dumped = envelope.model_dump(by_alias=True)
    assert dumped["from"] == "a" and dumped["payload"]["json"] == {"k": 1}
    # Taint transition data rides the tagged shape (S5).
    assert dumped["taint"] == {"kind": "Tainted", "source": "web.search"}


def test_judge_meta_split_matches_p0a_contract():
    # TYPE-3 attrs_optional split: GateFail carries judge.verdict, GatePass must not.
    assert "judge.verdict" in models.EVENT_ATTRS_OPTIONAL["GateFail"]
    assert "judge.verdict" not in models.EVENT_ATTRS_OPTIONAL["GatePass"]
    assert set(models.EVENT_ATTRS_REQUIRED["GateFail"]) == {"gate_id", "reason_code", "score"}


def test_gate_verdict_tagged_union():
    adapter = TypeAdapter(models.GateVerdict)
    assert adapter.validate_python({"kind": "Pass", "score": 0.9}).score == 0.9
    retry = adapter.validate_python(
        {"kind": "Retry", "reason_code": "TIMEOUT", "score": 0.4, "attempts_left": 1}
    )
    assert retry.attempts_left == 1
    try:
        adapter.validate_python({"kind": "Nope"})
    except pydantic.ValidationError:
        pass
    else:
        raise AssertionError("unknown GateVerdict variant accepted")


def test_reason_codes_are_exactly_six():
    assert set(models.REASON_CODES) == {
        "MAX_HANDOFFS",
        "TIMEOUT",
        "REPETITIVE_HANDOFF",
        "BUDGET_EXCEEDED",
        "GATE_REJECT",
        "PROVIDER_FAILURE",
    }


def test_field_absence_matrix_is_the_t4_oracle():
    assert models.FIELD_ABSENCE_MATRIX["goal_original"] == "compile_error"
    assert models.FIELD_ABSENCE_MATRIX["done_criteria"] == "reject_no_retry"
    assert models.FIELD_ABSENCE_MATRIX["artifacts"] == "post_gate_fail"
    assert models.FIELD_ABSENCE_MATRIX["failed_approaches"] == "warn_proceed"
