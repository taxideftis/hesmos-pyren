//! Provider/tool callback registration structures (PY-3/PY-4, PT-10 inversion).
//!
//! P0c scope: registration *structures only* — execution belongs to WP-P2b, where the
//! core owns the loop and invokes registered leaves (core owns flow, gates, records;
//! Python is a leaf, never the orchestrator). The registry keeps insertion order out
//! of the picture: lookup is by model_ref/tool name, nothing iterates it (P1 — no
//! HashMap-order decisions in deterministic paths).

use crate::marshal::CallbackMap;

// Unreachable from Python until the WP-P2b surface lands; kept structural per the
// P0c story (callbacks.rs "등록 구조만").
#[allow(dead_code)]
pub struct CallbackRegistry {
    /// model_ref (e.g. "glm-5.3-flash") -> callable(req: LlmRequest) -> LlmReply
    pub providers: CallbackMap,
    /// tool name (e.g. "write.file") -> callable(call: ToolCall) -> ToolResult
    pub tools: CallbackMap,
}

impl CallbackRegistry {
    #[allow(dead_code)] // constructed by the WP-P2b registration surface
    pub fn new() -> Self {
        Self {
            providers: CallbackMap::new(),
            tools: CallbackMap::new(),
        }
    }
}
