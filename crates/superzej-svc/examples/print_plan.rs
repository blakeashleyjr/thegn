//! Dump the real `envplan` provisioning scripts (nix install + git auth + clone)
//! so a live in-sandbox capstone run uses the exact, proven provisioning steps.
//!   cargo run -p superzej-svc --example print_plan -- <origin-url>
#![allow(clippy::disallowed_macros)]

use superzej_core::envplan::{EnvRequirements, PlanOpts, StepKind, plan};

fn main() {
    let origin = std::env::args().nth(1);
    let req = EnvRequirements {
        nix_flake_devshell: true,
        direnv: true,
        direnv_uses_flake: true,
        ..Default::default()
    };
    let opts = PlanOpts {
        origin,
        workdir: "/workspace".to_string(),
        checkpoint: false,
        ..Default::default()
    };
    for s in plan(&req, &opts).steps {
        if let StepKind::Exec(script) = s.kind {
            println!("##### STEP {} #####", s.id);
            println!("{script}");
            println!();
        }
    }
}
