//! Reverb recipe — `laravel/reverb`, supervised WebSocket server.
//!
//! Requires Laravel 11+ (pre-checked here so we surface a clean error rather than
//! composer's "Your requirements could not be resolved" wall of text — scratchpad 796
//! H2).

use anyhow::{Context, Result};
use tracing::info;

use super::recipe::{RecipeContext, RecipeOutcome, SupervisedSpec};
use super::{artisan, composer, env_file, laravel, AddAnswers};

const REVERB_MIN_LARAVEL: u32 = 11;

pub fn apply(ctx: &RecipeContext, answers: &AddAnswers) -> Result<RecipeOutcome> {
    let mut outcome = RecipeOutcome::default();

    // 0. Laravel-version pre-check. Prefer installed.json (exact); fall back to
    //    constraint major-floor parse. Skip when we genuinely can't determine
    //    (dev-main branches) so composer can do the arbitration.
    if let Some(installed) = laravel::installed_framework_version(&ctx.site_path) {
        if let Some(major) = installed.split('.').next().and_then(|s| s.parse::<u32>().ok())
            && major < REVERB_MIN_LARAVEL
        {
            anyhow::bail!(
                "laravel/reverb requires Laravel {REVERB_MIN_LARAVEL}+; found installed version {installed}"
            );
        }
    } else if let Ok(constraint) = laravel::framework_constraint(&ctx.site_path)
        && let Some(major) = laravel::constraint_major_floor(&constraint)
        && major < REVERB_MIN_LARAVEL
    {
        anyhow::bail!(
            "laravel/reverb requires Laravel {REVERB_MIN_LARAVEL}+; composer.json constraint resolves to {constraint}"
        );
    }

    // 1. Composer require.
    let already_required = laravel::package_required(&ctx.site_path, "laravel/reverb")
        .unwrap_or(false);
    if already_required {
        info!("laravel/reverb already in composer.json — skipping composer require");
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
                package: "laravel/reverb",
                dev: false,
                log_path: ctx.log_path.as_deref(),
                scan_env: ctx.scan_env.clone(),
            },
            ctx.dry_run,
        )?;
        if !result.success {
            anyhow::bail!(
                "composer require laravel/reverb failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
                result.exit_code,
                result.stdout,
                result.stderr
            );
        }
        outcome.composer_ran = true;
    }

    // 2. artisan reverb:install — publishes config + seeds REVERB_APP_* in .env.
    run_artisan_step(ctx, &mut outcome, &["reverb:install", "--no-interaction"])?;

    // 3. Resolve answers (with defaults).
    let host = answers
        .reverb_host
        .clone()
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let port = answers.reverb_port.unwrap_or(8080);
    let hostname = answers
        .reverb_hostname
        .clone()
        .unwrap_or_else(|| format!("{}.test", ctx.site_name));
    let scheme = answers
        .reverb_scheme
        .clone()
        .unwrap_or_else(|| "http".to_string());

    // 4. Patch .env. Preserve REVERB_APP_ID/KEY/SECRET written by reverb:install.
    let patches = vec![
        env_file::EnvPatch::set("REVERB_HOST", hostname.as_str()),
        env_file::EnvPatch::set("REVERB_PORT", port.to_string()),
        env_file::EnvPatch::set("REVERB_SCHEME", scheme.as_str()),
        env_file::EnvPatch::set("BROADCAST_CONNECTION", "reverb"),
    ];
    if ctx.dry_run {
        for p in &patches {
            outcome.env_keys_written.push(p.key.clone());
        }
    } else {
        let env_path = ctx.site_path.join(".env");
        let result = env_file::patch(&env_path, &patches, "reverb")?;
        outcome.env_keys_written.extend(result.keys_written);
        if let Some(b) = result.backup_path {
            outcome.backup_paths.push(b);
        }
    }

    // 5. Supervised spec.
    if !ctx.no_supervise {
        outcome.supervised = Some(SupervisedSpec {
            package: "reverb".to_string(),
            command: ctx.php_binary.to_string_lossy().to_string(),
            args: vec![
                "artisan".to_string(),
                "reverb:start".to_string(),
                format!("--host={host}"),
                format!("--port={port}"),
            ],
            cwd: ctx.site_path.clone(),
            site_name: ctx.site_name.clone(),
            php_version: ctx.php_version.clone(),
        });
    }

    if scheme == "https" {
        outcome.add_hint(format!(
            "REVERB_SCHEME=https set — make sure `valet secure {}` was run; nginx terminates TLS for `wss://`",
            ctx.site_name
        ));
    }
    outcome.add_hint(format!(
        "Reverb listening on {}:{} (announced as {scheme}://{hostname})",
        host, port
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

    fn make_skeleton(dir: &std::path::Path, laravel_constraint: &str, reverb_required: bool) {
        std::fs::create_dir_all(dir).unwrap();
        let composer = if reverb_required {
            format!(
                r#"{{"require":{{"laravel/framework":"{laravel_constraint}","laravel/reverb":"^1.0"}}}}"#
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
    fn reverb_recipe_bails_on_laravel_10_constraint() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^10.0", false);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "chat");
        let err = apply(&ctx, &AddAnswers::default()).unwrap_err();
        assert!(err.to_string().contains("Laravel 11"));
    }

    #[test]
    fn reverb_recipe_bails_on_installed_laravel_10() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", false);
        write_installed_framework(tmp.path(), "10.43.0");
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "chat");
        let err = apply(&ctx, &AddAnswers::default()).unwrap_err();
        assert!(err.to_string().contains("Laravel 11"));
    }

    #[test]
    fn reverb_recipe_passes_on_laravel_11() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "chat");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        assert!(outcome.composer_skipped);
    }

    #[test]
    fn reverb_recipe_emits_supervised_spec_with_host_and_port() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "chat");
        let mut answers = AddAnswers::default();
        answers.reverb_host = Some("127.0.0.1".to_string());
        answers.reverb_port = Some(8081);
        let outcome = apply(&ctx, &answers).unwrap();
        let spec = outcome.supervised.unwrap();
        assert_eq!(spec.package, "reverb");
        assert!(spec.args.iter().any(|a| a == "reverb:start"));
        assert!(spec.args.iter().any(|a| a == "--host=127.0.0.1"));
        assert!(spec.args.iter().any(|a| a == "--port=8081"));
        assert_eq!(spec.site_name, "chat");
    }

    #[test]
    fn reverb_recipe_writes_env_keys_including_broadcast_connection() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "chat");
        let outcome = apply(&ctx, &AddAnswers::default()).unwrap();
        for key in ["REVERB_HOST", "REVERB_PORT", "REVERB_SCHEME", "BROADCAST_CONNECTION"] {
            assert!(
                outcome.env_keys_written.iter().any(|k| k == key),
                "expected key {key} to be in env_keys_written, got {:?}",
                outcome.env_keys_written
            );
        }
    }

    #[test]
    fn reverb_recipe_https_warning_only_when_scheme_is_https() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        let ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "chat");
        let mut answers = AddAnswers::default();
        answers.reverb_scheme = Some("https".to_string());
        let outcome = apply(&ctx, &answers).unwrap();
        assert!(outcome.hints.iter().any(|h| h.contains("valet secure")));
    }

    #[test]
    fn reverb_recipe_uses_site_default_hostname_when_unset() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skeleton(tmp.path(), "^11.0", true);
        std::fs::write(tmp.path().join(".env"), "APP_ENV=local\n").unwrap();
        let mut ctx = RecipeContext::for_test(tmp.path().to_path_buf(), "chat");
        ctx.dry_run = false;
        ctx.php_binary = std::path::PathBuf::from("/usr/bin/true");
        let _ = apply(&ctx, &AddAnswers::default()).unwrap();
        let env = std::fs::read_to_string(tmp.path().join(".env")).unwrap();
        assert!(env.contains("REVERB_HOST=chat.test"));
    }
}
