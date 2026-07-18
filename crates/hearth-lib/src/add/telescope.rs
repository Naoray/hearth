//! Telescope recipe — `laravel/telescope`, request-driven middleware (no supervised worker).
//!
//! Scope cut per dissent H4/D4 in scratchpad 796: we do NOT regex-patch
//! `app/Providers/TelescopeServiceProvider::gate()`. PHP source patches across Laravel
//! minor versions are brittle; instead we print a one-line hint after install.

use anyhow::{Context, Result};
use tracing::info;

use super::recipe::{RecipeContext, RecipeOutcome};
use super::{artisan, composer, env_file, laravel, AddAnswers};

pub fn apply(ctx: &RecipeContext, answers: &AddAnswers) -> Result<RecipeOutcome> {
    let mut outcome = RecipeOutcome::default();

    // 1. Composer require (skip if already in composer.json — idempotent).
    let already_required = laravel::package_required(&ctx.site_path, "laravel/telescope")
        .unwrap_or(false);
    if already_required {
        info!("laravel/telescope already in composer.json — skipping composer require");
        outcome.composer_skipped = true;
    } else {
        let phar = ctx
            .composer_phar
            .as_ref()
            .context("composer.phar path not configured; re-run `hearth install`")?;
        let result = composer::run_require(
            composer::ComposerInvocation {
                site_path: &ctx.site_path,
                php_binary: &ctx.php_binary,
                composer_phar: phar,
                package: "laravel/telescope",
                dev: true,
                log_path: ctx.log_path.as_deref(),
                scan_env: ctx.scan_env.clone(),
            },
            ctx.dry_run,
        )?;
        if !result.success {
            anyhow::bail!(
                "composer require laravel/telescope failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
                result.exit_code,
                result.stdout,
                result.stderr
            );
        }
        outcome.composer_ran = true;
    }

    // 2. Artisan telescope:install (idempotent in modern Laravel with --no-interaction).
    run_artisan_step(ctx, &mut outcome, &["telescope:install"])?;

    // 3. artisan migrate --force (idempotent).
    run_artisan_step(ctx, &mut outcome, &["migrate", "--force"])?;

    // 4. Patch .env — TELESCOPE_ENABLED = true unless user explicitly opted out of every env.
    let enable = match &answers.telescope_environments {
        Some(envs) if envs.is_empty() => false,
        _ => true,
    };
    let env_path = ctx.site_path.join(".env");
    if ctx.dry_run {
        outcome
            .env_keys_written
            .push("TELESCOPE_ENABLED".to_string());
    } else {
        let result = env_file::patch(
            &env_path,
            &[env_file::EnvPatch::set(
                "TELESCOPE_ENABLED",
                if enable { "true" } else { "false" },
            )],
            "telescope",
        )?;
        outcome.env_keys_written.extend(result.keys_written);
        if let Some(backup) = result.backup_path {
            outcome.backup_paths.push(backup);
        }
    }

    // 5. Hints (replaces the dropped gate() PHP patch — scratchpad 796 H4).
    if !answers.telescope_enable_in_prod.unwrap_or(false) {
        outcome.add_hint(
            "Telescope is published. To gate it in production, edit \
             app/Providers/TelescopeServiceProvider::gate() — see Laravel docs.",
        );
    }
    outcome.add_hint(format!(
        "Open Telescope at https://{}.test/telescope",
        ctx.site_name
    ));

    Ok(outcome)
}

fn run_artisan_step(
    ctx: &RecipeContext,
    outcome: &mut RecipeOutcome,
    args: &[&str],
) -> Result<()> {
    let result = artisan::run(
        &ctx.site_path,
        &ctx.php_binary,
        args,
        ctx.dry_run,
        ctx.log_path.as_deref(),
        ctx.scan_env.as_ref(),
    )?;
    if !result.success {
        anyhow::bail!(
            "artisan {} failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            args.join(" "),
            result.exit_code,
            result.stdout,
            result.stderr
        );
    }
    outcome.artisan_calls.push(args.join(" "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_skeleton(dir: &std::path::Path, telescope_in_require_dev: bool) {
        std::fs::create_dir_all(dir).unwrap();
        let composer = if telescope_in_require_dev {
            r#"{"require":{"laravel/framework":"^11.0"},"require-dev":{"laravel/telescope":"^5.0"}}"#
        } else {
            r#"{"require":{"laravel/framework":"^11.0"}}"#
        };
        std::fs::write(dir.join("composer.json"), composer).unwrap();
    }

    #[test]
    fn telescope_recipe_runs_composer_when_not_already_required() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), false);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome.composer_ran);
        assert!(!outcome.composer_skipped);
    }

    #[test]
    fn telescope_recipe_skips_composer_when_already_required() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome.composer_skipped);
        assert!(!outcome.composer_ran);
    }

    #[test]
    fn telescope_recipe_runs_artisan_install_and_migrate() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome
            .artisan_calls
            .iter()
            .any(|s| s == "telescope:install"));
        assert!(outcome
            .artisan_calls
            .iter()
            .any(|s| s == "migrate --force"));
    }

    #[test]
    fn telescope_recipe_writes_TELESCOPE_ENABLED_true_by_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome
            .env_keys_written
            .iter()
            .any(|k| k == "TELESCOPE_ENABLED"));
    }

    #[test]
    fn telescope_recipe_disables_when_user_opts_out_of_all_envs() {
        // Live run (not dry) so the actual .env value is written and we can assert it.
        // telescope-in-require-dev = true ensures composer is skipped, so we don't need
        // a real composer.phar. Artisan calls use `true` as a fake PHP binary that
        // succeeds silently.
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        std::fs::write(tmp.path().join(".env"), "APP_ENV=local\n").unwrap();

        let mut ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        ctx.dry_run = false;
        ctx.php_binary = PathBuf::from("/usr/bin/true");
        let mut answers = AddAnswers::default();
        answers.telescope_environments = Some(Vec::new());

        let outcome = apply(&ctx, &answers).unwrap();
        let env = std::fs::read_to_string(tmp.path().join(".env")).unwrap();
        assert!(env.contains("TELESCOPE_ENABLED=false"));
        assert!(!outcome.backup_paths.is_empty());
    }

    #[test]
    fn telescope_recipe_adds_prod_gate_hint_when_not_enabled_in_prod() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome
            .hints
            .iter()
            .any(|h| h.contains("gate()")));
    }

    #[test]
    fn telescope_recipe_dashboard_hint_uses_site_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "shopfront");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome
            .hints
            .iter()
            .any(|h| h.contains("shopfront.test/telescope")));
    }

    #[test]
    fn telescope_recipe_errors_when_composer_phar_not_configured() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), false);
        let mut ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        ctx.composer_phar = None;
        let err = apply(&ctx, &AddAnswers::default()).unwrap_err();
        assert!(err.to_string().contains("composer.phar"));
    }

    #[test]
    fn telescope_recipe_returns_no_supervised_spec() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        let _outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(_outcome.supervised.is_none());
    }

    #[test]
    #[allow(non_snake_case)]
    fn telescope_recipe_writes_TELESCOPE_ENABLED_false_with_empty_envs_marker() {
        // Verify the planned key path for the dry-run case as well.
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        let mut answers = AddAnswers::default();
        answers.telescope_environments = Some(Vec::new());
        let outcome = apply(&ctx, &answers).unwrap();
        assert_eq!(outcome.env_keys_written, vec!["TELESCOPE_ENABLED"]);
    }

    #[test]
    fn telescope_recipe_uses_site_path_in_context_for_env() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        std::fs::write(tmp.path().join(".env"), "APP_ENV=local\n").unwrap();

        let mut ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "blog");
        ctx.dry_run = false;
        ctx.php_binary = PathBuf::from("/usr/bin/true");
        let _ = apply(&ctx, &AddAnswers::default()).unwrap();
        let env = std::fs::read_to_string(tmp.path().join(".env")).unwrap();
        assert!(env.contains("TELESCOPE_ENABLED=true"));
    }

}
