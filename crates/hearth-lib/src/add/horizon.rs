//! Horizon recipe — `laravel/horizon`, supervised queue worker.
//!
//! The recipe writes a `SupervisedSpec` into the outcome so the daemon can persist an
//! `AddedPackage` and register `ServiceKind::Horizon` with the supervisor.

use anyhow::{Context, Result};
use tracing::info;

use super::recipe::{RecipeContext, RecipeOutcome, SupervisedSpec};
use super::{artisan, composer, env_file, laravel, AddAnswers};

pub fn apply(ctx: &RecipeContext, answers: &AddAnswers) -> Result<RecipeOutcome> {
    let mut outcome = RecipeOutcome::default();

    // 1. Composer require (skip if already in composer.json).
    let already_required = laravel::package_required(&ctx.site_path, "laravel/horizon")
        .unwrap_or(false);
    if already_required {
        info!("laravel/horizon already in composer.json — skipping composer require");
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
                package: "laravel/horizon",
                dev: false,
                log_path: ctx.log_path.as_deref(),
                scan_env: ctx.scan_env.clone(),
            },
            ctx.dry_run,
        )?;
        if !result.success {
            anyhow::bail!(
                "composer require laravel/horizon failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
                result.exit_code,
                result.stdout,
                result.stderr
            );
        }
        outcome.composer_ran = true;
    }

    // 2. artisan horizon:install — publishes config + asset.
    run_artisan_step(ctx, &mut outcome, &["horizon:install"])?;

    // 3. Patch .env: QUEUE_CONNECTION + (only-if-missing) REDIS_CLIENT.
    let queue_connection = answers
        .horizon_connection
        .clone()
        .unwrap_or_else(|| "redis".to_string());
    let mut patches = vec![env_file::EnvPatch::set(
        "QUEUE_CONNECTION",
        queue_connection.as_str(),
    )];
    if queue_connection == "redis" {
        // Set only if user hasn't already chosen a client; phpredis is the Laravel default
        // and what `horizon:install` configures `config/database.php` to expect.
        patches.push(env_file::EnvPatch::set_if_missing("REDIS_CLIENT", "phpredis"));
    }
    if ctx.dry_run {
        for p in &patches {
            outcome.env_keys_written.push(p.key.clone());
        }
    } else {
        let env_path = ctx.site_path.join(".env");
        let result = env_file::patch(&env_path, &patches, "horizon")?;
        outcome.env_keys_written.extend(result.keys_written);
        if let Some(b) = result.backup_path {
            outcome.backup_paths.push(b);
        }
    }

    // 4. Supervised spec — `php artisan horizon`. Daemon converts to ServiceKind::Horizon,
    //    persists an AddedPackage, and registers a process-group child.
    if !ctx.no_supervise {
        outcome.supervised = Some(SupervisedSpec {
            package: "horizon".to_string(),
            command: ctx.php_binary.to_string_lossy().to_string(),
            args: vec!["artisan".to_string(), "horizon".to_string()],
            cwd: ctx.site_path.clone(),
            site_name: ctx.site_name.clone(),
            php_version: ctx.php_version.clone(),
        });
    }

    outcome.add_hint(format!(
        "Horizon dashboard: https://{}.test/horizon",
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

    fn make_skeleton(dir: &std::path::Path, horizon_required: bool) {
        std::fs::create_dir_all(dir).unwrap();
        let composer = if horizon_required {
            r#"{"require":{"laravel/framework":"^11.0","laravel/horizon":"^5.0"}}"#
        } else {
            r#"{"require":{"laravel/framework":"^11.0"}}"#
        };
        std::fs::write(dir.join("composer.json"), composer).unwrap();
    }

    #[test]
    fn horizon_recipe_skips_composer_when_already_required() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "shopfront");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome.composer_skipped);
    }

    #[test]
    fn horizon_recipe_writes_queue_connection_to_env() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        std::fs::write(tmp.path().join(".env"), "APP_ENV=local\nQUEUE_CONNECTION=sync\n").unwrap();

        let mut ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "shopfront");
        ctx.dry_run = false;
        ctx.php_binary = PathBuf::from("/usr/bin/true");
        let mut answers = AddAnswers::default();
        answers.horizon_connection = Some("redis".to_string());

        let _ = apply(&ctx, &answers).unwrap();
        let env = std::fs::read_to_string(tmp.path().join(".env")).unwrap();
        assert!(env.contains("QUEUE_CONNECTION=redis"));
    }

    #[test]
    fn horizon_recipe_sets_redis_client_only_if_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        std::fs::write(
            tmp.path().join(".env"),
            "APP_ENV=local\nREDIS_CLIENT=predis\n",
        )
        .unwrap();

        let mut ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "shopfront");
        ctx.dry_run = false;
        ctx.php_binary = PathBuf::from("/usr/bin/true");
        let mut answers = AddAnswers::default();
        answers.horizon_connection = Some("redis".to_string());

        let _ = apply(&ctx, &answers).unwrap();
        let env = std::fs::read_to_string(tmp.path().join(".env")).unwrap();
        assert!(env.contains("REDIS_CLIENT=predis"));
        assert!(!env.contains("phpredis"));
    }

    #[test]
    fn horizon_recipe_emits_supervised_spec_for_redis() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "shopfront");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        let spec = outcome.supervised.unwrap();
        assert_eq!(spec.package, "horizon");
        assert_eq!(spec.args, vec!["artisan", "horizon"]);
        assert_eq!(spec.site_name, "shopfront");
        assert_eq!(spec.cwd, tmp.path().to_path_buf());
    }

    #[test]
    fn horizon_recipe_skips_supervision_when_no_supervise() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let mut ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "shopfront");
        ctx.no_supervise = true;
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome.supervised.is_none());
    }

    #[test]
    fn horizon_recipe_runs_artisan_install() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "shopfront");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome
            .artisan_calls
            .iter()
            .any(|s| s == "horizon:install"));
    }

    #[test]
    fn horizon_recipe_dashboard_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "shopfront");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome
            .hints
            .iter()
            .any(|h| h.contains("shopfront.test/horizon")));
    }
}
