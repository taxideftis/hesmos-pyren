//! PORT-2 adapter — `bathos` CLI subprocess bridge (표 4, WP-P1e).
//!
//! Every contact with the bathos engine goes through THIS file and only as
//! subprocess calls (SS-23 rule 1, 표 12: shell out, never link or read engine
//! storage). The five command families map 1:1 onto [`BathosEngine`] methods:
//!
//! | method          | argv                                                     |
//! |-----------------|----------------------------------------------------------|
//! | state_init      | `state init <root>`                                       |
//! | state_validate  | `state validate`                                          |
//! | gate_verdict    | `gate verdict --id .. --verdict .. --critical ..` (+ raw report JSON on stdin) |
//! | gate_show       | `gate show --id ..`                                       |
//! | audit_append    | `audit append --actor hesmos --action trace.seal --target <chain_head>` |
//! | audit_verify    | `audit verify`                                            |
//! | model_validate  | `model validate`                                          |
//! | wave_activate   | `wave activate --index ..`                                |
//! | wave_show       | `wave show`                                               |
//!
//! Error philosophy — wrap, never reinterpret (exceptions.md §5): a non-zero bathos
//! exit becomes [`PlatformError`] with the exit code passthrough and, when bathos
//! prints its one JSON error line on stderr, the `code` field is lifted verbatim.
//! Hesmos error codes never replace bathos codes. Command-shaped calls (init /
//! verdict / append / activate) treat non-zero as an error; query-shaped calls
//! (validate / verify / model validate) REPORT the outcome as `ok: false` on a clean
//! exit — the caller (seal / CLI) decides what a failed verification means (exit 30).
//!
//! The exact bathos flag spellings are pinned by the unit tests via a stub script
//! that records its argv; the T11 integration tests re-prove the two audit calls
//! against a fake `bathos` end to end.
//!
//! ponytail: no subprocess timeout — a hung bathos hangs the session (P1 has no
//! concurrent sessions, so the blast radius is one interactive run). Upgrade
//! trigger: WP-P2e (real executors) or the gateway (HTTP-*), whichever lands first,
//! must add `wait_timeout` and kill + `PlatformError { bathos_exit: 124 }`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hesmos_core::{
    AuditPayload, BathosEngine, GateRecord, GateRecordId, GateReport, ModelReport, PlatformError,
    RawReport, StateReport, VerifyReport, WaveRef, WaveReport,
};

/// The bathos CLI adapter. `bin` is a binary name (resolved via PATH) or a path —
/// tests point it at stub scripts; the CLI defaults it to `"bathos"`.
pub struct BathosCli {
    bin: String,
    cwd: Option<PathBuf>,
}

impl BathosCli {
    pub fn new(bin: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            cwd: None,
        }
    }

    /// Working directory for every invocation (bathos state is per-directory).
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Runs the CLI with `args` and returns (exit code, stdout, stderr).
    fn run(&self, args: &[&str]) -> Result<(i32, String, String), PlatformError> {
        let mut command = Command::new(&self.bin);
        command.args(args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        // Report bodies can be large (gate raw reports) — pipe stdin for those and
        // capture both outputs; nothing here inherits the terminal.
        let output = command
            .stdin(Stdio::piped())
            .output()
            .map_err(|_| PlatformError {
                // Shell convention: 127 = command not found. There is no bathos code
                // to pass through — the engine never ran.
                bathos_exit: 127,
                bathos_code: None,
            })?;
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    /// Lifts the `code` field out of bathos's one-line JSON error stderr (if present).
    fn error(exit: i32, stdout: String, stderr: String) -> PlatformError {
        let bathos_code = serde_json::from_str::<serde_json::Value>(stderr.trim())
            .ok()
            .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(String::from));
        let _ = stdout; // kept in the signature for symmetric call sites
        PlatformError {
            bathos_exit: exit,
            bathos_code,
        }
    }

    /// stdout → RawReport: parsed as JSON when bathos printed JSON, else kept as a
    /// JSON string. Lossless either way — pass-through, no re-interpretation.
    fn raw(stdout: &str) -> RawReport {
        match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
            Ok(value) => RawReport(value),
            Err(_) => RawReport(serde_json::Value::String(stdout.trim_end().to_string())),
        }
    }

    /// Query-shaped call: reports `ok = (exit == 0)` with the raw body; a non-clean
    /// exit still carries the report body (bathos prints its JSON verdict either way).
    fn query(&self, args: &[&str]) -> Result<(bool, RawReport), PlatformError> {
        let (exit, stdout, _) = self.run(args)?;
        Ok((exit == 0, Self::raw(&stdout)))
    }
}

impl BathosEngine for BathosCli {
    fn state_init(&self, root: &Path) -> Result<StateReport, PlatformError> {
        let root_text = root.display().to_string();
        let (exit, stdout, stderr) = self.run(&["state", "init", root_text.as_str()])?;
        if exit != 0 {
            return Err(Self::error(exit, stdout, stderr));
        }
        Ok(StateReport {
            ok: true,
            raw: Self::raw(&stdout),
        })
    }

    fn state_validate(&self) -> Result<StateReport, PlatformError> {
        let (ok, raw) = self.query(&["state", "validate"])?;
        Ok(StateReport { ok, raw })
    }

    fn gate_verdict(&self, record: GateRecord) -> Result<(), PlatformError> {
        // The raw report travels on stdin: gate reports are arbitrary bathos JSON and
        // argv length limits are real. The stub tests assert both halves.
        use std::io::Write as _;
        let mut child = {
            let id = record.id.0.clone();
            let verdict = record.verdict.clone();
            let critical = record.critical.to_string();
            let mut command = Command::new(&self.bin);
            command
                .args([
                    "gate",
                    "verdict",
                    "--id",
                    id.as_str(),
                    "--verdict",
                    verdict.as_str(),
                    "--critical",
                    critical.as_str(),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(cwd) = &self.cwd {
                command.current_dir(cwd);
            }
            command.spawn().map_err(|_| PlatformError {
                bathos_exit: 127,
                bathos_code: None,
            })?
        };
        let report_json = serde_json::to_vec(&record.raw.0)
            .expect("RawReport is a serde_json::Value — always serializable");
        // A closed stdin (writer dropped) is fine — bathos may not read it; a broken
        // pipe on write means it closed early, which is bathos's prerogative.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&report_json);
        }
        let output = child.wait_with_output().map_err(|_| PlatformError {
            bathos_exit: 127,
            bathos_code: None,
        })?;
        if output.status.code().unwrap_or(-1) != 0 {
            return Err(Self::error(
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(())
    }

    fn gate_show(&self, id: &GateRecordId) -> Result<GateReport, PlatformError> {
        let id_text = id.0.clone();
        let (exit, stdout, stderr) = self.run(&["gate", "show", "--id", id_text.as_str()])?;
        if exit != 0 {
            return Err(Self::error(exit, stdout, stderr));
        }
        Ok(GateReport {
            raw: Self::raw(&stdout),
        })
    }

    fn audit_append(&self, payload: AuditPayload) -> Result<(), PlatformError> {
        // The seal payload is ONLY the chain head (SS-03 rule 1); actor/action are the
        // fixed vocabulary of the hesmos→bathos audit convention.
        let target = payload.chain_head_hash.as_str().to_string();
        let (exit, stdout, stderr) = self.run(&[
            "audit",
            "append",
            "--actor",
            "hesmos",
            "--action",
            "trace.seal",
            "--target",
            target.as_str(),
        ])?;
        if exit != 0 {
            return Err(Self::error(exit, stdout, stderr));
        }
        Ok(())
    }

    fn audit_verify(&self) -> Result<VerifyReport, PlatformError> {
        let (ok, raw) = self.query(&["audit", "verify"])?;
        Ok(VerifyReport { ok, raw })
    }

    fn model_validate(&self) -> Result<ModelReport, PlatformError> {
        let (ok, raw) = self.query(&["model", "validate"])?;
        Ok(ModelReport { ok, raw })
    }

    fn wave_activate(&self, wave: WaveRef) -> Result<(), PlatformError> {
        let index = wave.index.to_string();
        let (exit, stdout, stderr) = self.run(&["wave", "activate", "--index", index.as_str()])?;
        if exit != 0 {
            return Err(Self::error(exit, stdout, stderr));
        }
        Ok(())
    }

    fn wave_show(&self) -> Result<WaveReport, PlatformError> {
        let (_, stdout, _) = self.run(&["wave", "show"])?;
        Ok(WaveReport {
            raw: Self::raw(&stdout),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hesmos_core::Sha256Hex;
    use std::io::Read as _;

    /// A stub "bathos" binary: records its argv and stdin, then prints canned
    /// stdout/stderr and exits with a canned code — the argv contract above, pinned.
    ///
    /// Canned bodies travel as FILES (the script `cat`s them), never inline in the
    /// script text: JSON braces would break the shell quoting. Directories are unique
    /// per stub via an atomic counter — tests run in parallel and nanosecond
    /// timestamps alone have collided before.
    struct Stub {
        dir: PathBuf,
    }

    impl Stub {
        /// `exit` — process exit code; `stdout`/`stderr` — canned bodies; `read_stdin`
        /// — capture stdin into `stdin.txt` (needs the pipe closed by the caller).
        fn new(exit: i32, stdout: &str, stderr: &str, read_stdin: bool) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("hesmos-platform-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let capture = if read_stdin {
                "cat > \"$DIR/stdin.txt\" 2>/dev/null || true"
            } else {
                "true"
            };
            std::fs::write(dir.join("out.bin"), stdout).expect("write stdout body");
            std::fs::write(dir.join("err.bin"), stderr).expect("write stderr body");
            let script = format!(
                "#!/bin/bash\nDIR=\"{}\"\nprintf '%s\\n' \"$@\" > \"$DIR/argv.txt\"\n{capture}\ncat \"$DIR/out.bin\"\ncat \"$DIR/err.bin\" >&2\nexit {}\n",
                dir.display(),
                exit
            );
            let bin = dir.join("bathos-stub");
            std::fs::write(&bin, script).expect("write stub");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
            Self { dir }
        }

        fn cli(&self) -> BathosCli {
            BathosCli::new(self.dir.join("bathos-stub").display().to_string())
        }

        fn argv(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("argv.txt"))
                .expect("stub ran")
                .lines()
                .map(String::from)
                .collect()
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn head() -> Sha256Hex {
        Sha256Hex::parse("ab".repeat(32)).expect("hex")
    }

    /// audit_append carries exactly the seal vocabulary — actor, action, chain head.
    #[test]
    fn audit_append_uses_the_seal_argv() {
        let stub = Stub::new(0, "", "", false);
        stub.cli()
            .audit_append(AuditPayload {
                chain_head_hash: head(),
            })
            .expect("append ok");
        assert_eq!(
            stub.argv(),
            vec![
                "audit",
                "append",
                "--actor",
                "hesmos",
                "--action",
                "trace.seal",
                "--target",
                head().as_str(),
            ]
        );
    }

    /// gate_verdict passes the record fields on argv and the raw report on stdin.
    #[test]
    fn gate_verdict_passes_record_and_raw_report() {
        let stub = Stub::new(0, "", "", true);
        stub.cli()
            .gate_verdict(GateRecord {
                id: GateRecordId("g-1".into()),
                verdict: "PASS".into(),
                critical: 0,
                raw: RawReport(serde_json::json!({ "score": 1.0 })),
            })
            .expect("verdict ok");
        assert_eq!(
            stub.argv(),
            vec![
                "gate",
                "verdict",
                "--id",
                "g-1",
                "--verdict",
                "PASS",
                "--critical",
                "0"
            ]
        );
        let mut stdin = String::new();
        std::fs::File::open(stub.dir.join("stdin.txt"))
            .expect("stdin captured")
            .read_to_string(&mut stdin)
            .expect("read");
        let report: serde_json::Value = serde_json::from_str(&stdin).expect("json report");
        assert_eq!(report["score"], 1.0);
    }

    /// Query calls report ok=false on a clean failed check, and the raw JSON body
    /// survives untouched (pass-through, no reinterpretation).
    #[test]
    fn audit_verify_reports_ok_and_raw_body() {
        let stub = Stub::new(1, "{\"ok\":false,\"code\":\"E-AUDIT-BROKEN\"}", "", false);
        let report = stub.cli().audit_verify().expect("verify reports");
        assert!(!report.ok);
        assert_eq!(report.raw.0["code"], "E-AUDIT-BROKEN");

        let ok_stub = Stub::new(0, "{\"ok\":true}", "", false);
        let report = ok_stub.cli().audit_verify().expect("verify reports");
        assert!(report.ok);
    }

    /// Command-shaped calls error on non-zero exit, lifting bathos's `code` field
    /// from the stderr JSON line — pass-through, un-reinterpreted.
    #[test]
    fn command_failure_passes_exit_and_code_through() {
        let stub = Stub::new(3, "", "{\"code\":\"E-STATE-LOCKED\"}", false);
        let err = stub
            .cli()
            .state_init(Path::new("/tmp/whatever"))
            .expect_err("init failed");
        assert_eq!(err.bathos_exit, 3);
        assert_eq!(err.bathos_code.as_deref(), Some("E-STATE-LOCKED"));

        // state_init's argv carries the root as-is.
        assert_eq!(stub.argv(), vec!["state", "init", "/tmp/whatever"]);
    }

    /// A binary that cannot spawn is exit 127 with no code — the engine never ran.
    #[test]
    fn missing_binary_is_127() {
        let cli = BathosCli::new("hesmos-no-such-bathos-binary");
        let err = cli.audit_verify().expect_err("spawn fails");
        assert_eq!(err.bathos_exit, 127);
        assert_eq!(err.bathos_code, None);
    }

    /// wave_activate / gate_show / model_validate / state_validate argv + cwd.
    #[test]
    fn remaining_surfaces_pin_their_argv() {
        let stub = Stub::new(0, "{\"active\":0}", "", false);
        let cli = stub.cli().with_cwd(stub.dir.clone());

        cli.wave_activate(WaveRef { index: 2 }).expect("activate");
        assert_eq!(stub.argv(), vec!["wave", "activate", "--index", "2"]);

        cli.gate_show(&GateRecordId("g-9".into())).expect("show");
        assert_eq!(stub.argv(), vec!["gate", "show", "--id", "g-9"]);
        let shown = cli.gate_show(&GateRecordId("g-9".into())).expect("show");
        assert_eq!(shown.raw.0["active"], 0);

        let state = cli.state_validate().expect("validate reports");
        assert!(state.ok);
        let model = cli.model_validate().expect("model reports");
        assert!(model.ok);
        let wave = cli.wave_show().expect("wave show");
        assert_eq!(wave.raw.0["active"], 0);
    }
}
