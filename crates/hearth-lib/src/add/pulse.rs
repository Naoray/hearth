//! Pulse recipe — `laravel/pulse`, install-only (no supervised worker in v0.3.0).
//!
//! Requires Laravel 10+. Unparseable constraints (`dev-main`, `dev-master`) are
//! treated as "version unknown" and let composer arbitrate — scratchpad 796 H1.

use anyhow::{Context, Result};
use tracing::info;

use super::recipe::{RecipeContext, RecipeOutcome};
use super::{artisan, composer, env_file, laravel, AddAnswers};

const PULSE_MIN_LARAVEL: u32 = 10;

pub fn apply(ctx: &RecipeContext, answers: &AddAnswers) -> Result<RecipeOutcome> {
    let mut outcome = RecipeOutcome::default();

    // 0. Laravel-version pre-check.
    if let Some(installed) = laravel::installed_framework_version(&ctx.site_path) {
        if let Some(major) = installed.split('.').next().and_then(|s| s.parse::<u32>().ok())
            && major < PULSE_MIN_LARAVEL
        {
            anyhow::bail!(
                "laravel/pulse requires Laravel {PULSE_MIN_LARAVEL}+; found installed version {installed}"
            );
        }
    } else if let Ok(constraint) = laravel::framework_constraint(&ctx.site_path)
        && let Some(major) = laravel::constraint_major_floor(&constraint)
        && major < PULSE_MIN_LARAVEL
    {
        anyhow::bail!(
            "laravel/pulse requires Laravel {PULSE_MIN_LARAVEL}+; composer.json constraint resolves to {constraint}"
        );
    }
    // Unparseable constraint (dev-main, dev-master) → no bail; composer arbitrates.

    // 1. Composer require.
    let already_required = laravel::package_required(&ctx.site_path, "laravel/pulse")
        .unwrap_or(false);
    if already_required {
        info!("laravel/pulse already in composer.json — skipping composer require");
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
                package: "laravel/pulse",
                dev: false,
                log_path: ctx.log_path.as_deref(),
            },
            ctx.dry_run,
        )?;
        if !result.success {
            anyhow::bail!(
                "composer require laravel/pulse failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
                result.exit_code,
                result.stdout,
                result.stderr
            );
        }
        outcome.composer_ran = true;
    }

    // 2. Vendor publish + migrate.
    run_artisan_step(
        ctx,
        &mut outcome,
        &["vendor:publish", "--tag=pulse-config", "--no-interaction"],
    )?;
    run_artisan_step(
        ctx,
        &mut outcome,
        &["vendor:publish", "--tag=pulse-migrations", "--no-interaction"],
    )?;
    run_artisan_step(ctx, &mut outcome, &["migrate", "--force"])?;

    // 3. Patch .env — PULSE_INGEST_DRIVER + PULSE_STORAGE_DRIVER.
    let driver = answers
        .pulse_storage_driver
        .clone()
        .unwrap_or_else(|| "database".to_string());
    let patches = vec![
        env_file::EnvPatch::set("PULSE_INGEST_DRIVER", driver.as_str()),
        env_file::EnvPatch::set("PULSE_STORAGE_DRIVER", driver.as_str()),
    ];
    if ctx.dry_run {
        for p in &patches {
            outcome.env_keys_written.push(p.key.clone());
        }
    } else {
        let env_path = ctx.site_path.join(".env");
        let result = env_file::patch(&env_path, &patches, "pulse")?;
        outcome.env_keys_written.extend(result.keys_written);
        if let Some(b) = result.backup_path {
            outcome.backup_paths.push(b);
        }
    }

    // 4. Hints — pulse:check supervised worker is intentionally out of v0.3.0 scope.
    outcome.add_hint(format!(
        "Pulse dashboard: https://{}.test/pulse",
        ctx.site_name
    ));
    outcome.add_hint(
        "Pulse `pulse:check` recorder is not supervised in v0.3.0. Run it manually via \
         `php artisan pulse:check` when you want background sampling.",
    );

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

    fn make_skeleton(dir: &std::path::Path, laravel_constraint: &str, pulse_required: bool) {
        std::fs::create_dir_all(dir).unwrap();
        let composer = if pulse_required {
            format!(
                r#"{{"require":{{"laravel/framework":"{laravel_constraint}","laravel/pulse":"^1.0"}}}}"#
            )
        } else {
            format!(r#"{{"require":{{"laravel/framework":"{laravel_constraint}"}}}}"#)
        };
        std::fs::write(dir.join("composer.json"), composer).unwrap();
    }

    fn write_installed_framework(dir: &std::path::Path, version: &str) {
        let vendor = dir.join("vendor/composer");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(
            vendor.join("installed.json"),
            format!(
                r#"{{"packages":[{{"name":"laravel/framework","version":"{version}","version_normalized":"{version}.0"}}]}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn pulse_recipe_bails_on_laravel_9_constraint() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^9.0", false);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "app");
        let err = apply(&ctx, &AddAnswers::default()).unwrap_err();
        assert!(err.to_string().contains("Laravel 10"));
    }

    #[test]
    fn pulse_recipe_bails_on_installed_laravel_9() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^10.0", false);
        write_installed_framework(tmp.path(), "9.52.0");
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "app");
        let err = apply(&ctx, &AddAnswers::default()).unwrap_err();
        assert!(err.to_string().contains("Laravel 10"));
    }

    #[test]
    fn pulse_recipe_lets_composer_arbitrate_dev_main() {
        // dev-main constraint is unparseable; recipe must NOT bail — let composer decide.
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "dev-main", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "app");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome.composer_skipped);
    }

    #[test]
    fn pulse_recipe_passes_on_laravel_10() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^10.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "app");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome.composer_skipped);
    }

    #[test]
    fn pulse_recipe_writes_storage_driver_env_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "app");
        let mut answers = AddAnswers::default();
        answers.pulse_storage_driver = Some("redis".to_string());
        let outcome = apply(&ctx, &answers).unwrap();
        assert!(outcome.env_keys_written.contains(&"PULSE_INGEST_DRIVER".to_string()));
        assert!(outcome.env_keys_written.contains(&"PULSE_STORAGE_DRIVER".to_string()));
    }

    #[test]
    fn pulse_recipe_returns_no_supervised_spec() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "app");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome.supervised.is_none());
    }

    #[test]
    fn pulse_recipe_includes_publish_artisan_calls() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "app");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome
            .artisan_calls
            .iter()
            .any(|s| s.contains("pulse-config")));
        assert!(outcome
            .artisan_calls
            .iter()
            .any(|s| s.contains("pulse-migrations")));
        assert!(outcome
            .artisan_calls
            .iter()
            .any(|s| s.contains("migrate")));
    }

    #[test]
    fn pulse_recipe_hint_mentions_pulse_check_deferral() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "app");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome
            .hints
            .iter()
            .any(|h| h.contains("pulse:check")));
    }
}
