//! CLI-5 `hesmos eval` — dispatch skeleton only; the golden-suite implementation is
//! WP-P3a (eval 하네스·--bless·회귀 exit 20). The catalog message names the WP so the
//! user knows when to expect it (SS-13 rule 5 — P1 does not fake UX maturity).

use crate::messages;

pub fn dispatch() -> i32 {
    crate::cmd::run::emit_error(&hesmos_core::HesmosError {
        class: hesmos_core::ErrorClass::Usage,
        code: "USAGE-STATE".into(),
        session_id: None,
        node_id: None,
        message: messages::eval_not_implemented(),
        hint: None,
    });
    crate::exit::EXIT_USAGE
}
