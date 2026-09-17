"""hesmos.eval_judge — the optional LLM quality judge (PY-8, US-26, SS-22 judge부).

The judge is SELECTIVE by contract (rules first): it runs only where a session
registers one (`@core.judge(config)` decorating an LLM callable), and a session
without a registration produces zero judge.* attrs and zero judge calls.

Drift tracking (US-26 AC2) rests on the registration-time metadata this package
fixes: sha256 of the prompt text (judge.prompt_hash), the FIXED temperature
(judge.temperature — None is forbidden and refused HERE, before any call), and
the model version (judge.model_version). Changing any of the three between two
evals is then visible by comparing gate-event attrs — no log forensics.

The judgment itself reuses the agent path (ADR-0005 rule 4): the decorated
callable is an ordinary LlmRequest -> LlmReply function — in production the
P2c GLM adapter, in tests a stub. No second model path exists.
"""

from .bridge import JudgeConfig, VERDICT_SCORE, make_bridge, parse_verdict

__all__ = ["JudgeConfig", "make_bridge", "parse_verdict", "VERDICT_SCORE"]
