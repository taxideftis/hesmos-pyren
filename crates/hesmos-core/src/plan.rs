//! TYPE-6 Plan / StageSpec / AgentProfile / BudgetEnvelope — the compiled plan's data.
//!
//! These are the data sources for TRAIT-1..2; they hold no logic. Collections are BTree
//! so serialization (→ plan_hash, → waves) is order-fixed (project-context §2). A node
//! without a profile cannot even be expressed here (field is mandatory) — CE-06 remains
//! as the compile-time check for the *unvalidated YAML surface* before this struct exists.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::contract::DoneCriteria;
use crate::ids::{AgentRole, ModelRef, NodeId, SchemaId, TeamId, ToolName};
use crate::policy::PolicyOverrides;

/// The 4 templates (표 9). Extending this set is a spec revision, not a code change
/// (AP-13: the swarms 14-SwarmType combinatorial explosion is the counter-example).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    Sequential,
    Parallel,
    Swarm,
    Graph,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
}

/// Gate references per phase — pre/post PolicySet references (SS-09 rule 1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateSpec {
    pub pre: BTreeSet<String>,
    pub post: BTreeSet<String>,
}

/// Resource ceilings. `timeout_ms` is a parameter with NO default value (W3-Part1:
/// charter §6 freezes the numbers; unset means budget-bound, never an invented default).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub max_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
}

/// Network scope, allowlist-only (P7 / 표 3: denylist shell is a violation).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkScope {
    pub allow: BTreeSet<String>,
}

/// The agent profile — mandatory per node (SS-19 rule 1). `temperature` should be fixed
/// (Option = unset) because it feeds reproduction directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub role: AgentRole,
    pub model: ModelRef,
    pub temperature: Option<f32>,
    /// Tool allowlist (BTreeSet — order fixed for hashes).
    pub tools: BTreeSet<ToolName>,
    pub resource_limits: ResourceLimits,
    pub network: NetworkScope,
    /// Over-delegation threshold: spawning a subtask whose expected tokens fall below
    /// this floor is blocked (SS-12 rule 3).
    pub spawn_token_floor: u64,
}

/// Per-stage declaration. Logic-free on purpose (TRAIT-1: the LLM/tool calls live in the
/// Python layer; this is the narrow prompt + narrow toolset spec).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    pub id: NodeId,
    pub profile: AgentProfile,
    pub input_schema: SchemaId,
    pub done_criteria: DoneCriteria,
    pub gates: GateSpec,
    /// Only terminal nodes may declare "we are done" (SS-12 rule 1).
    pub is_terminal: bool,
    /// Explicit router node — the only sender of Control envelopes / routing decisions
    /// (SS-04 rule 4).
    pub is_router: bool,
}

/// TRAIT-1 — the execution-unit contract: data accessors only, no logic. Implemented by
/// [`StageSpec`] (orchestrator builds stages from `Plan.stages`; user Rust code has no
/// direct-impl path in P1, per the contract).
pub trait Stage {
    fn id(&self) -> &NodeId;
    fn profile(&self) -> &AgentProfile;
    fn input_schema(&self) -> &SchemaId;
    fn done_criteria(&self) -> &DoneCriteria;
}

impl Stage for StageSpec {
    fn id(&self) -> &NodeId {
        &self.id
    }
    fn profile(&self) -> &AgentProfile {
        &self.profile
    }
    fn input_schema(&self) -> &SchemaId {
        &self.input_schema
    }
    fn done_criteria(&self) -> &DoneCriteria {
        &self.done_criteria
    }
}

/// Plan-level budget declaration (the `--budget key=val` surface / plan YAML). Frozen
/// into a [`BudgetEnvelope`] at session open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSpec {
    pub session_max_tokens: Option<u64>,
    pub team_max_tokens: Option<u64>,
    /// Absent in YAML = empty map (no per-agent caps) — a semantic default, not an
    /// invented number.
    #[serde(default)]
    pub agent_max_tokens: BTreeMap<AgentRole, u64>,
}

/// The session-frozen budget envelope (SS-15 rule 1). Percent defaults are charter §6
/// values — 80 warn / 100 suspend; no other numbers may appear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetEnvelope {
    pub session_max_tokens: Option<u64>,
    pub team_max_tokens: Option<u64>,
    /// Same semantic default as [`BudgetSpec::agent_max_tokens`].
    #[serde(default)]
    pub agent_max_tokens: BTreeMap<AgentRole, u64>,
    pub warn_pct: u8,
    pub suspend_pct: u8,
}

impl Default for BudgetEnvelope {
    fn default() -> Self {
        Self {
            session_max_tokens: None,
            team_max_tokens: None,
            agent_max_tokens: BTreeMap::new(),
            warn_pct: 80,
            suspend_pct: 100,
        }
    }
}

/// Key-scoped read grants (§5.3): knowledge key → nodes allowed to read it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeSpec {
    pub grants: BTreeMap<String, BTreeSet<NodeId>>,
}

/// Plan — the compilation input. `task` is the goal_original source (TYPE-4 invariant 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub name: String,
    pub task: String,
    pub team_id: Option<TeamId>,
    pub pattern: PatternKind,
    pub stages: Vec<StageSpec>,
    pub edges: Vec<Edge>,
    pub budget: BudgetSpec,
    pub knowledge: KnowledgeSpec,
    pub policy_overrides: PolicyOverrides,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical Plan schema round-trips from a complete YAML document. The contract's
    /// `plan_yaml_example` is deliberately ABBREVIATED (`profile: {model, tools}`,
    /// `depends:` sugar) — that raw surface is WP-P1a's compile input, whose structs fold
    /// it into this canonical form before a `Plan` exists. The canonical schema itself
    /// stays strict: every mandatory AgentProfile field must be present.
    #[test]
    fn plan_canonical_yaml_roundtrip() {
        let yaml = r#"
name: report
task: Draft a competitive feature report
pattern: graph
stages:
  - id: research
    profile:
      role: researcher
      model: glm-5.3-flash
      temperature: 0.0
      tools: [web.search, docs.read]
      resource_limits: {max_tokens: 8000, timeout_ms: 60000}
      network: {allow: [docs.corp.internal]}
      spawn_token_floor: 500
    input_schema: research.v1
    done_criteria:
      items: [notes complete]
    gates: {pre: [], post: []}
    is_terminal: false
    is_router: false
  - id: draft
    profile:
      role: writer
      model: glm-5.3-flash
      tools: [write.file]
      resource_limits: {}
      network: {allow: []}
      spawn_token_floor: 500
    input_schema: draft.v1
    done_criteria:
      items: [draft exists]
    gates: {pre: [], post: [rubric.v1]}
    is_terminal: false
    is_router: false
edges:
  - from: research
    to: draft
budget:
  session_max_tokens: 250000
knowledge:
  grants: {}
policy_overrides: {}
"#;
        let plan: Plan = serde_yaml_ng::from_str(yaml).expect("parse canonical surface");
        assert_eq!(plan.stages.len(), 2);
        assert_eq!(plan.stages[0].profile.model.as_str(), "glm-5.3-flash");
        assert_eq!(plan.edges.len(), 1);
        assert_eq!(
            plan.stages[1].gates.post.iter().next().map(String::as_str),
            Some("rubric.v1")
        );
        // Mandatory-profile invariant: the abbreviated contract example (no
        // resource_limits/network/spawn_token_floor) must NOT parse here — the compile
        // layer expands it, the canonical schema refuses it (CE-06 upstream).
        let abbreviated = r#"
name: t
task: t
pattern: graph
stages:
  - id: research
    profile: {model: glm-5.3-flash, tools: [web.search]}
    input_schema: research.v1
    done_criteria: {items: [x]}
    gates: {pre: [], post: []}
    is_terminal: false
    is_router: false
edges: []
budget: {}
knowledge: {grants: {}}
policy_overrides: {}
"#;
        assert!(
            serde_yaml_ng::from_str::<Plan>(abbreviated).is_err(),
            "abbreviated profile is the compile layer's input, not the canonical schema"
        );
    }

    /// Envelope defaults are the charter §6 numbers — 80/100 and nothing else.
    #[test]
    fn budget_envelope_defaults_match_charter() {
        let b = BudgetEnvelope::default();
        assert_eq!((b.warn_pct, b.suspend_pct), (80, 100));
    }
}
