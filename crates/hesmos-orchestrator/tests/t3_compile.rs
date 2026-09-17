//! T3 tests for the raw-surface fold: CE-01..09 detection (each with its position /
//! guidance), flow DSL grammar, and pattern normalization. CE-03 lives in engine tests
//! (waves construction); CE-02 covers depends, flow, and explicit edges.

use hesmos_core::{CompileError, PatternKind};
use hesmos_orchestrator::parse_plan;

fn stage(id: &str, extra: &str) -> String {
    format!(
        r#"
  - id: {id}
    profile: {{role: writer, model: glm-5.3-flash}}
    input_schema: {id}.v1
    done_criteria: {{items: [done]}}
{extra}"#
    )
}

fn plan_with(flow: &str, stages: Vec<String>, pattern: Option<&str>) -> String {
    let pattern_line = pattern
        .map(|p| format!("pattern: {p}\n"))
        .unwrap_or_default();
    format!(
        "name: t\ntask: do the thing\n{pattern_line}stages:
{}
flow: \"{flow}\"
",
        stages.join("\n")
    )
}

fn minimal_plan() -> String {
    plan_with("a -> b", vec![stage("a", ""), stage("b", "")], None)
}

#[test]
fn canonical_plan_folds_from_raw_surface() {
    let plan = parse_plan(&minimal_plan()).expect("compile");
    assert_eq!(plan.stages.len(), 2);
    assert_eq!(plan.edges.len(), 1, "depends/flow folded into one edge");
    assert_eq!(
        plan.pattern,
        PatternKind::Sequential,
        "inferred: single path"
    );
    assert_eq!(plan.stages[0].profile.model.as_str(), "glm-5.3-flash");
}

/// CE-01 — schema violations carry the location.
#[test]
fn ce01_schema_parse_reports_location() {
    let bad = "name: t\ntask: t\nstages: []\nunknown_key: 1\n";
    let err = parse_plan(bad).expect_err("unknown key");
    match err {
        CompileError::SchemaParse { location } => {
            assert!(location.contains("plan"), "location prefixed: {location}");
            assert_eq!(CompileError::SchemaParse { location }.code(), "CE-01");
        }
        other => panic!("expected CE-01, got {other:?}"),
    }
}

/// CE-02 — unknown depends/flow/edge references, referrer named.
#[test]
fn ce02_unknown_node_reference() {
    let yaml = plan_with("a -> ghost", vec![stage("a", "")], None);
    let err = parse_plan(&yaml).expect_err("ghost reference");
    match err {
        CompileError::UnknownNodeRef { node, refs } => {
            assert_eq!(node, "<flow>");
            assert_eq!(refs, vec!["ghost".to_string()]);
        }
        other => panic!("expected CE-02, got {other:?}"),
    }

    let yaml2 = plan_with("", vec![stage("a", "    depends: [phantom]")], None);
    let err2 = parse_plan(&yaml2).expect_err("phantom depends");
    match err2 {
        CompileError::UnknownNodeRef { node, refs } => {
            assert_eq!(node, "a");
            assert_eq!(refs, vec!["phantom".to_string()]);
        }
        other => panic!("expected CE-02, got {other:?}"),
    }
}

/// CE-03 is exercised in engine tests (cycle extraction at DAG build).
/// Here: an acyclic plan of the same shape compiles.
#[test]
fn ce03_acyclic_shapes_compile_here() {
    assert!(parse_plan(&minimal_plan()).is_ok());
}

/// CE-04 — DSL outside the `a -> b, c` grammar is rejected with the expression.
#[test]
fn ce04_invalid_dsl_grammar() {
    for bad in ["a => b", "a -> , b", "a -> ; b -> c", "-> b"] {
        let yaml = plan_with(bad, vec![stage("a", ""), stage("b", "")], None);
        let err = parse_plan(&yaml).expect_err(bad);
        assert!(
            matches!(err, CompileError::InvalidDsl { .. }),
            "expected CE-04 for `{bad}`, got {err:?}"
        );
    }
}

/// CE-05 — missing/blank task = no goal_original.
#[test]
fn ce05_missing_goal_original() {
    let no_task = "name: t\nstages: []\n";
    assert!(matches!(
        parse_plan(no_task),
        Err(CompileError::MissingGoalOriginal)
    ));
    let blank = "name: t\ntask: \"   \"\nstages: []\n";
    assert!(matches!(
        parse_plan(blank),
        Err(CompileError::MissingGoalOriginal)
    ));
}

/// CE-06 — profile present but incomplete (no role). The contract's plan_yaml_example
/// fragment abbreviates the profile — such a fragment is exactly CE-06 at compile time.
#[test]
fn ce06_incomplete_profile() {
    let yaml = r#"
name: t
task: do the thing
stages:
  - id: a
    profile: {model: glm-5.3-flash, tools: [web.search]}
    input_schema: a.v1
    done_criteria: {items: [done]}
"#;
    let err = parse_plan(yaml).expect_err("no role");
    match err {
        CompileError::ProfileMissing { node } => assert_eq!(node, "a"),
        other => panic!("expected CE-06, got {other:?}"),
    }
}

/// CE-07 — declared pattern contradicted by edges.
#[test]
fn ce07_pattern_normalization_failure() {
    // Sequential declared, but the graph branches.
    let yaml = plan_with(
        "a -> b, c",
        vec![stage("a", ""), stage("b", ""), stage("c", "")],
        Some("sequential"),
    );
    let err = parse_plan(&yaml).expect_err("branch under sequential");
    match err {
        CompileError::PatternNormalizationFailed { reason } => {
            assert!(reason.contains("linear path"), "{reason}");
            assert_eq!(
                CompileError::PatternNormalizationFailed { reason }.code(),
                "CE-07"
            );
        }
        other => panic!("expected CE-07, got {other:?}"),
    }

    // Parallel declared with edges.
    let yaml2 = plan_with(
        "a -> b",
        vec![stage("a", ""), stage("b", "")],
        Some("parallel"),
    );
    assert!(matches!(
        parse_plan(&yaml2),
        Err(CompileError::PatternNormalizationFailed { .. })
    ));

    // Swarm declared without a router node.
    let yaml3 = plan_with(
        "a -> b",
        vec![stage("a", ""), stage("b", "")],
        Some("swarm"),
    );
    assert!(matches!(
        parse_plan(&yaml3),
        Err(CompileError::PatternNormalizationFailed { .. })
    ));
}

/// CE-08 — duplicate node ids.
#[test]
fn ce08_duplicate_node_id() {
    let yaml = plan_with("", vec![stage("a", ""), stage("a", "")], None);
    let err = parse_plan(&yaml).expect_err("duplicate");
    match err {
        CompileError::DuplicateNodeId { node } => assert_eq!(node, "a"),
        other => panic!("expected CE-08, got {other:?}"),
    }
}

/// CE-09 — budget spec malformed (its own code + `--budget key=val` guidance).
#[test]
fn ce09_invalid_budget_spec() {
    let yaml = format!(
        "{}\nbudget: {{tokens: not-a-number}}\n",
        plan_with("", vec![stage("a", "")], None)
    );
    let err = parse_plan(&yaml).expect_err("bad budget");
    assert_eq!(err.code(), "CE-09", "budget errors get their own code");
}

/// DSL fan-out: `a -> b, c` produces both edges; multiple statements accumulate.
/// Statement separators: `;` works inside a double-quoted YAML scalar; a raw newline
/// only survives as a YAML *block* scalar (`flow: |`) — double-quoted scalars fold
/// newlines into spaces, so `parse_flow` never sees them there.
#[test]
fn flow_dsl_fanout_and_multi_statement() {
    let semicolons = plan_with(
        "a -> b, c; c -> d",
        vec![
            stage("a", ""),
            stage("b", ""),
            stage("c", ""),
            stage("d", ""),
        ],
        None,
    );
    let plan = parse_plan(&semicolons).expect("compile");
    assert_eq!(plan.edges.len(), 3, "a->b, a->c, c->d");
    assert_eq!(
        plan.pattern,
        PatternKind::Graph,
        "fan-out is not a single path"
    );

    let block_scalar = format!(
        "name: t\ntask: do the thing\nstages:\n{}\nflow: |\n  a -> b, c\n  c -> d\n",
        [
            stage("a", ""),
            stage("b", ""),
            stage("c", ""),
            stage("d", "")
        ]
        .join("\n")
    );
    let plan2 = parse_plan(&block_scalar).expect("compile");
    assert_eq!(
        plan2.edges.len(),
        3,
        "newline-separated statements fold the same"
    );
}

/// Parallel inference: no edges → Parallel (seeded merge order territory).
#[test]
fn edgeless_plan_infers_parallel() {
    let yaml = plan_with("", vec![stage("a", ""), stage("b", "")], None);
    let plan = parse_plan(&yaml).expect("compile");
    assert_eq!(plan.pattern, PatternKind::Parallel);
}
