"""hesmos.glm — the GLM provider adapter (PY-3 real implementation).

Resolver seat: SS-17/SS-26 "3 API 모드 리졸버" is intentionally NOT implemented —
SS-26 moved past E3 (W3-부3). This package holds the single glm adapter only;
the resolver slot opens here if/when the multi-provider contract is revised.
"""

from hesmos.glm.adapter import ENDPOINT, MODEL_REF, GlmAdapter, GlmResponseError, model_plan_conformance

__all__ = ["ENDPOINT", "MODEL_REF", "GlmAdapter", "GlmResponseError", "model_plan_conformance"]
