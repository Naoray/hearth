//! `hearth add` — guided installer for Laravel packages (Horizon, Telescope, Pulse, Reverb).
//!
//! Architecture: CLI owns interactivity (dialoguer), daemon receives an `Add` request
//! with a pre-populated `AddAnswers`. Recipes are free functions per package — no
//! trait abstraction (see dissent D1 in scratchpad 796).

pub mod artisan;
pub mod composer;
pub mod env_file;
pub mod laravel;
pub mod prompt;
pub mod recipe;
pub mod site_context;

use serde::{Deserialize, Serialize};

use recipe::{RecipeContext, RecipeOutcome};

/// User-provided answers to recipe prompts.
///
/// Each recipe reads only the fields it needs; unused fields stay `None`. Explicit
/// fields (rather than a HashMap) so the daemon ↔ CLI wire shape is type-checked.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AddAnswers {
    // Horizon
    pub horizon_connection: Option<String>,
    pub horizon_environment: Option<String>,
    pub horizon_max_processes: Option<i64>,

    // Telescope
    pub telescope_environments: Option<Vec<String>>,
    pub telescope_enable_in_prod: Option<bool>,

    // Pulse
    pub pulse_storage_driver: Option<String>,

    // Reverb
    pub reverb_host: Option<String>,
    pub reverb_port: Option<u16>,
    pub reverb_hostname: Option<String>,
    pub reverb_scheme: Option<String>,
}

/// Recipe dispatch by package key.
///
/// Each per-package recipe registers here as it lands. Telescope arrives in Task 2,
/// Horizon in Task 4, etc. Returning a `bail!` for unrecognized keys gives a clear
/// error path during foundation testing.
pub fn apply_recipe(
    package: &str,
    _ctx: &RecipeContext,
    _answers: &AddAnswers,
) -> anyhow::Result<RecipeOutcome> {
    match package {
        // "telescope" => telescope::apply(_ctx, _answers),
        // "horizon"   => horizon::apply(_ctx, _answers),
        // "pulse"     => pulse::apply(_ctx, _answers),
        // "reverb"    => reverb::apply(_ctx, _answers),
        other => anyhow::bail!("unknown or not-yet-implemented package: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn add_answers_default_is_all_none() {
        let a = AddAnswers::default();
        assert!(a.horizon_connection.is_none());
        assert!(a.telescope_enable_in_prod.is_none());
        assert!(a.reverb_port.is_none());
    }

    #[test]
    fn add_answers_round_trip_serde() {
        let a = AddAnswers {
            horizon_connection: Some("redis".to_string()),
            horizon_max_processes: Some(8),
            reverb_port: Some(8081),
            ..Default::default()
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: AddAnswers = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn apply_recipe_errors_on_unknown_package() {
        let ctx = RecipeContext::for_test(PathBuf::from("/tmp/x"), "x");
        let err = apply_recipe("not-a-package", &ctx, &AddAnswers::default()).unwrap_err();
        assert!(err.to_string().contains("unknown"));
    }
}
