//! Plan compilation: raw YAML surface → canonical [`Plan`] (the CE-01..09 checkpoint).
//!
//! Two surfaces, one truth:
//! - **Raw surface** (this file's `Raw*` structs): the plan YAML users write — the
//!   contract's `plan_yaml_example` spellings (`profile: {model, tools}`,
//!   `depends:` sugar, `flow:` DSL, `gates` inside or beside the profile). It is
//!   deliberately forgiving about *shape shorthand* but strict about *unknown keys*.
//! - **Canonical surface** ([`Plan`] in hesmos-core): the single downstream form — every
//!   shorthand folded, every default explicit, every reference resolved.
//!
//! Detection order is fixed (first error wins, deterministic): CE-01 parse/schema →
//! CE-08 duplicate id → CE-06 incomplete profile → CE-05 missing goal → CE-04 DSL
//! grammar → CE-02 unknown references → CE-09 budget → CE-07 pattern normalization.
//! Cycle detection (CE-03) happens one step later, when the engine builds the DAG.
//!
//! All of this runs before any session exists (US-02 AC1): a rejected plan creates no
//! events and runs no nodes.

use std::collections::{BTreeMap, BTreeSet};

use hesmos_core::{
    AgentProfile, AgentRole, BudgetSpec, CompileError, DoneCriteria, Edge, GateSpec, KnowledgeSpec,
    ModelRef, NetworkScope, NodeId, PatternKind, Plan, PolicyOverrides, ResourceLimits, SchemaId,
    StageSpec, TeamId, ToolName,
};

/// Parses plan YAML (the raw surface) into the canonical [`Plan`].
pub fn parse_plan(yaml: &str) -> Result<Plan, CompileError> {
    // CE-01: any structural mismatch (unknown key, wrong type, bad YAML) is a schema
    // parse error; serde_yaml_ng's location is carried through to the user.
    let raw: RawPlan = serde_yaml_ng::from_str(yaml).map_err(|e| CompileError::SchemaParse {
        location: format!("plan: {}", e),
    })?;
    fold(raw)
}

// ---------------------------------------------------------------------------
// Raw surface
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlan {
    name: String,
    /// Optional here so its ABSENCE can be reported as CE-05 (not a generic CE-01).
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    /// Absent = inferred during normalization (see `normalize_pattern`).
    #[serde(default)]
    pattern: Option<PatternKind>,
    stages: Vec<RawStage>,
    /// flow DSL, e.g. `"research -> draft, verify\ndraft -> final"` (CE-04 grammar).
    #[serde(default)]
    flow: Option<String>,
    /// Canonical escape hatch: explicit edges, same meaning as after folding.
    #[serde(default)]
    edges: Option<Vec<Edge>>,
    /// Parsed separately so a malformed budget is CE-09 (its own code + guidance),
    /// not a generic schema error.
    #[serde(default)]
    budget: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    knowledge: Option<KnowledgeSpec>,
    #[serde(default)]
    policy_overrides: Option<PolicyOverrides>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStage {
    id: String,
    profile: RawProfile,
    /// Required (CE-01 when absent): the schema gate (SS-09) needs a key to check.
    input_schema: String,
    /// Required (CE-01 when absent): "no criteria" is a decision, not a default.
    done_criteria: DoneCriteria,
    #[serde(default)]
    depends: Option<Vec<String>>,
    #[serde(default)]
    gates: Option<RawGates>,
    #[serde(default)]
    is_terminal: Option<bool>,
    #[serde(default)]
    is_router: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    /// Optional here so its absence/incompleteness is CE-06 (compile-time profile
    /// detection, exceptions.md §3 primary defense), not a generic CE-01.
    #[serde(default)]
    role: Option<String>,
    model: String,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    tools: Option<BTreeSet<String>>,
    /// Contract fragment spelling: `profile: {model, gates: [rubric.v1]}`.
    #[serde(default)]
    gates: Option<Vec<String>>,
    #[serde(default)]
    resource_limits: Option<RawLimits>,
    #[serde(default)]
    network: Option<RawNetwork>,
    #[serde(default)]
    spawn_token_floor: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    #[serde(default)]
    allow: Option<BTreeSet<String>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGates {
    #[serde(default)]
    pre: Option<Vec<String>>,
    #[serde(default)]
    post: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    #[serde(default)]
    session_max_tokens: Option<u64>,
    #[serde(default)]
    team_max_tokens: Option<u64>,
    #[serde(default)]
    agent_max_tokens: Option<BTreeMap<String, u64>>,
}

// ---------------------------------------------------------------------------
// Fold + validation (fixed detection order)
// ---------------------------------------------------------------------------

fn fold(raw: RawPlan) -> Result<Plan, CompileError> {
    // CE-08 — duplicate node ids (before anything else references them).
    let mut seen = BTreeSet::new();
    for stage in &raw.stages {
        if !seen.insert(stage.id.as_str()) {
            return Err(CompileError::DuplicateNodeId {
                node: stage.id.clone(),
            });
        }
    }

    // CE-06 — profile present but incomplete (role is the mandatory identity field).
    for stage in &raw.stages {
        if stage
            .profile
            .role
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            return Err(CompileError::ProfileMissing {
                node: stage.id.clone(),
            });
        }
    }

    // CE-05 — goal_original missing or blank: no contract can ever be created.
    let task = match raw.task.as_deref().map(str::trim) {
        None | Some("") => return Err(CompileError::MissingGoalOriginal),
        Some(t) => t.to_string(),
    };

    // CE-04 — DSL grammar, then CE-02 — all references must exist.
    let flow_edges = raw.flow.as_deref().map(parse_flow).transpose()?;
    check_refs(&raw, &flow_edges)?;

    // CE-09 — budget sub-grammar with its own guidance (`--budget key=val`).
    let budget = match &raw.budget {
        None => BudgetSpec::default(),
        Some(value) => {
            let rb: RawBudget = serde_yaml_ng::from_value(value.clone()).map_err(|_| {
                CompileError::InvalidBudgetSpec // guidance: `--budget key=val 나열`
            })?;
            BudgetSpec {
                session_max_tokens: rb.session_max_tokens,
                team_max_tokens: rb.team_max_tokens,
                agent_max_tokens: rb
                    .agent_max_tokens
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| (AgentRole::new(k), v))
                    .collect(),
            }
        }
    };

    // Edge union: explicit `edges:` ∪ per-stage `depends` ∪ `flow` DSL. Duplicates
    // collapse (a BTreeSet of (from,to) pairs), order is derived by key.
    let mut edge_set: BTreeSet<(String, String)> = BTreeSet::new();
    for e in raw.edges.into_iter().flatten() {
        edge_set.insert((e.from.as_str().to_string(), e.to.as_str().to_string()));
    }
    for stage in &raw.stages {
        for dep in stage.depends.iter().flatten() {
            edge_set.insert((dep.clone(), stage.id.clone()));
        }
    }
    for (from, to) in flow_edges.into_iter().flatten() {
        edge_set.insert((from, to));
    }
    let edges: Vec<Edge> = edge_set
        .into_iter()
        .map(|(from, to)| Edge {
            from: NodeId::new(from),
            to: NodeId::new(to),
        })
        .collect();

    let stages = raw
        .stages
        .into_iter()
        .map(fold_stage)
        .collect::<Result<Vec<_>, _>>()?;

    // CE-07 — declared (or inferred) pattern must match the edge shape (표 9).
    let pattern = match raw.pattern {
        Some(p) => {
            check_pattern(&p, &stages, &edges)?;
            p
        }
        None => infer_pattern(&stages, &edges)?,
    };

    Ok(Plan {
        name: raw.name,
        task,
        team_id: raw.team_id.map(TeamId::new),
        pattern,
        stages,
        edges,
        budget,
        knowledge: raw.knowledge.unwrap_or_default(),
        policy_overrides: raw.policy_overrides.unwrap_or_default(),
    })
}

/// Folds one raw stage into its canonical form; role was already CE-06-checked.
fn fold_stage(raw: RawStage) -> Result<StageSpec, CompileError> {
    let profile = raw.profile;
    let role = profile.role.expect("checked in fold (CE-06)");
    // Gates may be spelled beside the profile keys or under it; both fold into one
    // GateSpec (post entries dedupe via the BTreeSet).
    let mut post: BTreeSet<String> = profile.gates.unwrap_or_default().into_iter().collect();
    let (pre, stage_post): (BTreeSet<String>, BTreeSet<String>) = match raw.gates {
        None => (BTreeSet::new(), BTreeSet::new()),
        Some(g) => (
            g.pre.unwrap_or_default().into_iter().collect(),
            g.post.unwrap_or_default().into_iter().collect(),
        ),
    };
    post.extend(stage_post);
    Ok(StageSpec {
        id: NodeId::new(raw.id),
        profile: AgentProfile {
            role: AgentRole::new(role),
            model: ModelRef::new(profile.model),
            temperature: profile.temperature,
            tools: profile
                .tools
                .unwrap_or_default()
                .into_iter()
                .map(ToolName::new)
                .collect(),
            resource_limits: profile
                .resource_limits
                .map(|l| ResourceLimits {
                    max_tokens: l.max_tokens,
                    timeout_ms: l.timeout_ms,
                })
                .unwrap_or_default(),
            network: profile
                .network
                .map(|n| NetworkScope {
                    allow: n.allow.unwrap_or_default().into_iter().collect(),
                })
                .unwrap_or_default(),
            spawn_token_floor: profile.spawn_token_floor.unwrap_or(0),
        },
        input_schema: SchemaId::new(raw.input_schema),
        done_criteria: raw.done_criteria,
        gates: GateSpec { pre, post },
        is_terminal: raw.is_terminal.unwrap_or(false),
        is_router: raw.is_router.unwrap_or(false),
    })
}

/// CE-02 — every referenced name (depends / flow / explicit edges) must be a stage.
fn check_refs(
    raw: &RawPlan,
    flow_edges: &Option<Vec<(String, String)>>,
) -> Result<(), CompileError> {
    let known: BTreeSet<&str> = raw.stages.iter().map(|s| s.id.as_str()).collect();
    let mut missing_by_referrer: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();

    for stage in &raw.stages {
        for dep in stage.depends.iter().flatten() {
            if !known.contains(dep.as_str()) {
                missing_by_referrer
                    .entry(stage.id.as_str())
                    .or_default()
                    .insert(dep.clone());
            }
        }
    }
    for (from, to) in flow_edges.iter().flatten() {
        for name in [from, to] {
            if !known.contains(name.as_str()) {
                missing_by_referrer
                    .entry("<flow>")
                    .or_default()
                    .insert(name.clone());
            }
        }
    }
    for e in raw.edges.iter().flatten() {
        for name in [e.from.as_str(), e.to.as_str()] {
            if !known.contains(name) {
                missing_by_referrer
                    .entry("<edges>")
                    .or_default()
                    .insert(name.to_string());
            }
        }
    }

    if let Some((node, refs)) = missing_by_referrer.iter().next() {
        return Err(CompileError::UnknownNodeRef {
            node: (*node).to_string(),
            refs: refs.iter().cloned().collect(),
        });
    }
    Ok(())
}

/// Parses the flow DSL. Grammar (CE-04 outside it):
/// `flow  := stmt ((';' | newline) stmt)*`
/// `stmt := node ('->' node (',' node)*)?`      — `a -> b, c` ≡ a→b, a→c
/// `node := [A-Za-z0-9_.-]+`
fn parse_flow(flow: &str) -> Result<Vec<(String, String)>, CompileError> {
    let mut edges = Vec::new();
    for stmt in flow.split([';', '\n']) {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        let mut sides = stmt.splitn(2, "->");
        let lhs = sides.next().unwrap_or("").trim();
        if let Some(rhs) = sides.next() {
            validate_ident(lhs, stmt)?;
            let targets = rhs.split(',');
            for target in targets {
                let target = target.trim();
                validate_ident(target, stmt)?;
                edges.push((lhs.to_string(), target.to_string()));
            }
        } else {
            // Bare node statement: declares an isolated participant, no edges.
            validate_ident(lhs, stmt)?;
        }
    }
    Ok(edges)
}

fn validate_ident(token: &str, stmt: &str) -> Result<(), CompileError> {
    let ok = !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(CompileError::InvalidDsl {
            expression: stmt.to_string(),
        })
    }
}

/// CE-07 — the declared pattern must be expressible by this edge shape (표 9).
/// `pub(crate)`: the engine re-runs it as defense for hand-built `Plan` values.
pub(crate) fn check_pattern(
    pattern: &PatternKind,
    stages: &[StageSpec],
    edges: &[Edge],
) -> Result<(), CompileError> {
    let fail = |reason: String| CompileError::PatternNormalizationFailed { reason };
    match pattern {
        // 완전 결정: single path covering every node.
        PatternKind::Sequential => {
            let mut indeg: BTreeMap<&NodeId, usize> = stages.iter().map(|s| (&s.id, 0)).collect();
            let mut outdeg: BTreeMap<&NodeId, usize> = stages.iter().map(|s| (&s.id, 0)).collect();
            for e in edges {
                *outdeg
                    .get_mut(&e.from)
                    .ok_or_else(|| fail("edge endpoint missing".into()))? += 1;
                *indeg
                    .get_mut(&e.to)
                    .ok_or_else(|| fail("edge endpoint missing".into()))? += 1;
            }
            let roots = indeg.values().filter(|&&d| d == 0).count();
            let linears = stages
                .iter()
                .all(|s| indeg[&s.id] <= 1 && outdeg[&s.id] <= 1);
            if roots != 1 || !linears || edges.len() + 1 != stages.len() {
                return Err(fail(
                    "sequential template requires one linear path over all stages".into(),
                ));
            }
            Ok(())
        }
        // 독립 병렬: no edges — one wave, seeded merge order.
        PatternKind::Parallel => {
            if !edges.is_empty() {
                return Err(fail(
                    "parallel template takes no edges (merge order is seed-fixed)".into(),
                ));
            }
            Ok(())
        }
        // 경로 런타임 결정: at least one explicit router node (SS-04 rule 4).
        PatternKind::Swarm => {
            if !stages.iter().any(|s| s.is_router) {
                return Err(fail(
                    "swarm template requires at least one router node (is_router)".into(),
                ));
            }
            Ok(())
        }
        // Any DAG (cycle rejection already handled by wave construction).
        PatternKind::Graph => Ok(()),
    }
}

/// Pattern inference when the plan omits `pattern:` — most specific template that fits.
fn infer_pattern(stages: &[StageSpec], edges: &[Edge]) -> Result<PatternKind, CompileError> {
    if edges.is_empty() {
        return Ok(PatternKind::Parallel);
    }
    let single_path = {
        let mut indeg: BTreeMap<&NodeId, usize> = stages.iter().map(|s| (&s.id, 0)).collect();
        let mut outdeg: BTreeMap<&NodeId, usize> = stages.iter().map(|s| (&s.id, 0)).collect();
        for e in edges {
            *outdeg.get_mut(&e.from).expect("refs checked") += 1;
            *indeg.get_mut(&e.to).expect("refs checked") += 1;
        }
        stages
            .iter()
            .all(|s| indeg[&s.id] <= 1 && outdeg[&s.id] <= 1)
            && edges.len() + 1 == stages.len()
    };
    if single_path {
        return Ok(PatternKind::Sequential);
    }
    if stages.iter().any(|s| s.is_router) {
        return Ok(PatternKind::Swarm);
    }
    Ok(PatternKind::Graph)
}
