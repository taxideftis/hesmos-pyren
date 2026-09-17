"""hesmos.memory — persistence with taint filtering (PY-8, SS-20 rule 2·3).

The core decides taint (PT-12); this layer CONSUMES the core's marking as the
persistence verdict: a Tainted entry is refused, the store keeps zero residual,
and the refusal is returned as a record (evidence, not an exception — a tainted
read is a data-flow fact, not a crash).
"""

from .persist import MemoryStore

__all__ = ["MemoryStore"]
