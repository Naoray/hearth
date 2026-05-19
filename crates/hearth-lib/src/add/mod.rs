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
pub mod telescope;

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
    ctx: &RecipeContext,
    answers: &AddAnswers,
) -> anyhow::Result<RecipeOutcome> {
    match package {
        "telescope" => telescope::apply(ctx, answers),
        // "horizon"   => horizon::apply(ctx, answers),
        // "pulse"     => pulse::apply(ctx, answers),
        // "reverb"    => reverb::apply(ctx, answers),
        other => anyhow::bail!("unknown or not-yet-implemented package: {other}"),
    }
}

/// Render a `RecipeOutcome` as a user-facing block. Daemon returns this as the body
/// of `DaemonResponse::Ok { message }`.
pub fn format_outcome(package: &str, outcome: &RecipeOutcome) -> String {
    let mut lines = Vec::new();
    lines.push(format!("hearth add {package} — done"));
    if outcome.composer_ran {
        lines.push(format!("► composer require laravel/{package}"));
    } else if outcome.composer_skipped {
        lines.push(format!("► composer require skipped (laravel/{package} already required)"));
    }
    for call in &outcome.artisan_calls {
        lines.push(format!("► php artisan {call}"));
    }
    if !outcome.env_keys_written.is_empty() {
        lines.push(format!(
            "► .env patched ({})",
            outcome.env_keys_written.join(", ")
        ));
    }
    for backup in &outcome.backup_paths {
        lines.push(format!("  backup: {}", backup.display()));
    }
    if let Some(spec) = &outcome.supervised {
        lines.push(format!(
            "► supervised: {}[{}] — {} {}",
            spec.package,
            spec.site_name,
            spec.command,
            spec.args.join(" ")
        ));
    }
    for hint in &outcome.hints {
        lines.push(format!("ℹ {hint}"));
    }
    lines.join("\n")
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

    #[test]
    fn format_outcome_includes_each_step() {
        let mut outcome = RecipeOutcome::default();
        outcome.composer_ran = true;
        outcome.artisan_calls.push("telescope:install".to_string());
        outcome.env_keys_written.push("TELESCOPE_ENABLED".to_string());
        outcome.add_hint("Open https://blog.test/telescope");
        let s = format_outcome("telescope", &outcome);
        assert!(s.contains("composer require"));
        assert!(s.contains("php artisan telescope:install"));
        assert!(s.contains("TELESCOPE_ENABLED"));
        assert!(s.contains("Open https://blog.test/telescope"));
    }

    #[test]
    fn format_outcome_shows_composer_skip() {
        let mut outcome = RecipeOutcome::default();
        outcome.composer_skipped = true;
        let s = format_outcome("telescope", &outcome);
        assert!(s.contains("composer require skipped"));
    }

    #[test]
    fn apply_recipe_dispatches_telescope() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("composer.json"),
            r#"{"require":{"laravel/framework":"^11.0"},"require-dev":{"laravel/telescope":"^5.0"}}"#,
        )
        .unwrap();
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "test-app");
        let outcome = apply_recipe("telescope", &ctx, &AddAnswers::default()).unwrap();
        // Already in require-dev → composer skipped, but artisan + env still scheduled.
        assert!(outcome.composer_skipped);
        assert!(outcome
            .artisan_calls
            .iter()
            .any(|s| s == "telescope:install"));
    }
}
