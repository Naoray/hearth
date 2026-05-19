//! Recipe context + outcome types.
//!
//! Per-package work is implemented as free functions (`apply_telescope`,
//! `apply_horizon`, ...) rather than a trait — see dissent D1 in
//! `solo://proj/22/scratchpad/adversarial-review-h--796`. Keeping the surface as
//! plain types keeps test setup trivial and avoids `Box<dyn Recipe>` machinery for
//! a closed 4-package enum.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

/// Inputs to a recipe — site root, PHP binary, dry-run flag.
///
/// Constructed by the daemon's `Add` handler from a resolved `SiteContext` plus the
/// CLI-supplied flags. Recipes receive a borrow, never mutate it.
#[derive(Debug, Clone)]
pub struct RecipeContext {
    pub site_path: PathBuf,
    pub site_name: String,
    pub php_binary: PathBuf,
    /// Absolute path to a Composer phar. Resolved at `hearth install` time and persisted
    /// in `HearthConfig.composer_phar`. `None` triggers a clear error in composer.rs.
    pub composer_phar: Option<PathBuf>,
    /// If true, register the package install but skip supervisor registration.
    /// Only meaningful for Horizon/Reverb; ignored by install-only recipes.
    pub no_supervise: bool,
    /// If true, print actions but touch no files, run no commands.
    pub dry_run: bool,
    /// Optional log file for composer/artisan tail. CLI surfaces this to the user.
    pub log_path: Option<PathBuf>,
    /// Used as the `installed_at` for any persisted `AddedPackage`.
    pub now: DateTime<Utc>,
}

impl RecipeContext {
    /// Minimal builder used in unit tests where most fields are irrelevant.
    pub fn for_test(site_path: PathBuf, site_name: impl Into<String>) -> Self {
        Self {
            site_path,
            site_name: site_name.into(),
            php_binary: PathBuf::from("/usr/bin/php"),
            composer_phar: None,
            no_supervise: false,
            dry_run: true,
            log_path: None,
            now: Utc::now(),
        }
    }
}

/// Description of a long-running worker the recipe wants the supervisor to register.
/// Returned in `RecipeOutcome.supervised` when applicable. The daemon converts the
/// `package` field into a `ServiceKind` and persists an `AddedPackage` in config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisedSpec {
    /// "horizon" | "reverb" — used to choose the supervised `ServiceKind`.
    pub package: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Site name; lets `hearth status` disambiguate multiple supervised rows of the same
    /// package across different Laravel sites (e.g. `horizon[shopfront]`).
    pub site_name: String,
}

/// What a recipe actually did — used by the daemon to build the user-facing summary.
#[derive(Debug, Default, Clone)]
pub struct RecipeOutcome {
    pub composer_ran: bool,
    pub composer_skipped: bool,
    pub env_keys_written: Vec<String>,
    pub artisan_calls: Vec<String>,
    pub supervised: Option<SupervisedSpec>,
    pub hints: Vec<String>,
    pub backup_paths: Vec<PathBuf>,
}

impl RecipeOutcome {
    pub fn add_hint(&mut self, hint: impl Into<String>) {
        self.hints.push(hint.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_context_for_test_defaults_dry_run() {
        let ctx = RecipeContext::for_test(PathBuf::from("/tmp/x"), "x");
        assert!(ctx.dry_run);
        assert_eq!(ctx.site_name, "x");
    }

    #[test]
    fn recipe_outcome_default_is_empty() {
        let o = RecipeOutcome::default();
        assert!(!o.composer_ran);
        assert!(o.env_keys_written.is_empty());
        assert!(o.supervised.is_none());
    }

    #[test]
    fn recipe_outcome_add_hint_appends() {
        let mut o = RecipeOutcome::default();
        o.add_hint("try /horizon");
        o.add_hint("more");
        assert_eq!(o.hints, vec!["try /horizon", "more"]);
    }
}
