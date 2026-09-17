"""The judge bridge: config enforcement + verdict parsing (US-26, SS-22 rule 3).

Policy lives HERE, before any call (story D4 — Thomas N7-(b)): the temperature
is a REQUIRED config field, so a judge that cannot state its temperature cannot
register — an unrecorded judgment is unrepresentable, and the core stays
untouched (TYPE-3 optional attrs keep their optional nature; P0a schema frozen).

Verdict convention (deliberately boring): the judge reply's content must START
with one of PASS / CONCERNS / FAIL (case-insensitive). Anything else is a
ValueError that surfaces as the payload's judge_error record and degrades the
node to rules-only — a bridge that guessed a verdict would fabricate quality
data, which is exactly what the convention exists to prevent.
"""

from __future__ import annotations

import hashlib
from typing import Any, Callable

from .. import models

# The three-level vocabulary (ui-spec §7.3 renders it: "판정 CONCERNS").
VERDICTS = ("PASS", "CONCERNS", "FAIL")
VERDICT_SCORE = {"PASS": 1.0, "CONCERNS": 0.5, "FAIL": 0.0}


class JudgeConfig:
    """Frozen judge configuration — the drift-tracking identity of a judge.

    temperature is REQUIRED and must be a finite number (fixed 0.0 recommended):
    `None` is refused at construction, so a registered judge always carries a
    concrete judge.temperature value to compare across evals (AC2).
    """

    __slots__ = ("model_ref", "prompt", "temperature", "model_version", "max_tokens")

    def __init__(
        self,
        *,
        model_ref: str,
        prompt: str,
        temperature: float,
        model_version: str,
        max_tokens: int = 512,
    ) -> None:
        if not model_ref:
            raise ValueError("judge model_ref is required (single-backend path, ADR-0005)")
        if not prompt or not prompt.strip():
            raise ValueError("judge prompt must be non-empty — its sha256 is the record")
        if temperature is None or not isinstance(temperature, (int, float)):
            # The D4 policy check: None is NOT defaulted and NOT omitted — it is
            # refused, because a drift comparison against a missing value is void.
            raise ValueError(
                "judge temperature must be a fixed number (None forbidden — drift "
                "tracking needs a concrete recorded value; fixed 0.0 recommended)"
            )
        if not model_version:
            raise ValueError("judge model_version is required (US-26 AC1 record)")
        self.model_ref = model_ref
        self.prompt = prompt
        self.temperature = float(temperature)
        self.model_version = model_version
        self.max_tokens = max_tokens

    @property
    def prompt_hash(self) -> str:
        """sha256 of the prompt TEXT (story §6 — TYPE-1 Sha256Hex, raw utf-8)."""
        return hashlib.sha256(self.prompt.encode("utf-8")).hexdigest()

    def build_request(self, output: str) -> dict[str, Any]:
        """The judge LlmRequest for one candidate output.

        temperature is taken from the CONFIG, never from the caller: the judge's
        sampling is the recorded sampling, byte-identical across evals.
        """
        return {
            "messages": [
                {"role": "user", "content": f"{self.prompt}\n\nOUTPUT:\n{output}"}
            ],
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "tools_schema": [],
            "cache_breakpoints": {"stable": 0, "context": 0, "volatile": 1},
        }


def parse_verdict(content: str) -> str:
    """PASS / CONCERNS / FAIL from the reply's first word; anything else refuses."""
    first = content.strip().split(" ", 1)[0].upper().rstrip(".,;:")
    if first not in VERDICTS:
        raise ValueError(
            f"judge reply must start with one of {', '.join(VERDICTS)} — got: "
            f"{content[:40]!r}"
        )
    return first


def make_bridge(
    config: JudgeConfig, llm: Callable[[Any], Any]
) -> Callable[[str], dict[str, Any]]:
    """Binds an LLM callable into the judge bridge the FFI executor invokes.

    `llm` is an ordinary provider-shaped callable (LlmRequest -> LlmReply) — the
    P2c GlmAdapter in production, a stub in tests. The returned bridge takes the
    node OUTPUT and answers with the mechanical facts the FFI records:
    {verdict, score, tokens_in, tokens_out}. The verdict parse happens HERE so
    the boundary never interprets free text (backend.md consensus b).
    """

    def bridge(output: str) -> dict[str, Any]:
        request = models.LlmRequest.model_validate(config.build_request(output))
        reply = models.LlmReply.model_validate(llm(request)).model_dump()
        verdict = parse_verdict(reply["content"])
        return {
            "verdict": verdict,
            "score": VERDICT_SCORE[verdict],
            "tokens_in": reply["tokens_in"],
            "tokens_out": reply["tokens_out"],
        }

    return bridge
