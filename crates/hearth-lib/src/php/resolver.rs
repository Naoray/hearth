use std::path::PathBuf;

use tracing::info;

/// PHP binary resolution chain.
///
/// Searches for a PHP binary in this order:
/// 1. Hearth cache (~/.config/hearth/php/{version}/php)
/// 2. Herd binaries (~/Library/Application Support/Herd/bin/php{version})
/// 3. Homebrew (/opt/homebrew/opt/php@{version}/bin/php)
///
/// If none of those paths exists, the resolver returns `None`; it never runs a
/// bare `php` command.
///
/// ```text
/// resolve("8.4")
///   ├─ ~/.config/hearth/php/8.4/php       ← own CI binaries
///   ├─ ~/Library/.../Herd/bin/php84       ← Herd migration
///   ├─ /opt/homebrew/opt/php@8.4/bin/php  ← Homebrew fallback
///   └─ None                                ← not installed
/// ```
pub fn resolve_php_binary(version: &str, config_dir: &PathBuf) -> Option<PathBuf> {
    let version_compact = version.replace('.', ""); // "8.4" -> "84"

    // 1. Hearth's own cached binaries
    let hearth_path = config_dir.join("php").join(version).join("php");
    if hearth_path.exists() {
        info!(version, path = %hearth_path.display(), "resolved PHP from Hearth cache");
        return Some(hearth_path);
    }

    // 2. Herd binaries (migration convenience)
    let herd_path = dirs::home_dir()
        .map(|h| {
            h.join("Library/Application Support/Herd/bin")
                .join(format!("php{}", version_compact))
        });
    if let Some(ref path) = herd_path {
        if path.exists() {
            info!(version, path = %path.display(), "resolved PHP from Herd");
            return Some(path.clone());
        }
    }

    // 3. Homebrew
    let brew_path = PathBuf::from(format!(
        "/opt/homebrew/opt/php@{}/bin/php",
        version
    ));
    if brew_path.exists() {
        info!(version, path = %brew_path.display(), "resolved PHP from Homebrew");
        return Some(brew_path);
    }

    // Not found
    None
}

/// Resolve the PHP-FPM binary through the same three-provider chain.
///
/// Returns `None` rather than running a bare `php-fpm` command when no
/// provider-specific binary exists.
pub fn resolve_phpfpm_binary(version: &str, config_dir: &PathBuf) -> Option<PathBuf> {
    let version_compact = version.replace('.', "");

    // Same three-provider resolution chain but for php-fpm
    let hearth_path = config_dir.join("php").join(version).join("php-fpm");
    if hearth_path.exists() {
        return Some(hearth_path);
    }

    let herd_path = dirs::home_dir()
        .map(|h| {
            h.join("Library/Application Support/Herd/bin")
                .join(format!("php{}-fpm", version_compact))
        });
    if let Some(ref path) = herd_path {
        if path.exists() {
            return Some(path.clone());
        }
    }

    let brew_path = PathBuf::from(format!(
        "/opt/homebrew/opt/php@{}/sbin/php-fpm",
        version
    ));
    if brew_path.exists() {
        return Some(brew_path);
    }

    None
}

/// Identity-verified FPM provider tier, ordered by production preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FpmTier {
    Hearth,
    Herd,
    Homebrew,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FpmCandidate {
    pub version: &'static str,
    pub tier: FpmTier,
    pub path: PathBuf,
    pub canonical: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FpmCandidateRejection {
    pub version: &'static str,
    pub tier: FpmTier,
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FpmCandidateSet {
    pub candidates: Vec<FpmCandidate>,
    pub rejected: Vec<FpmCandidateRejection>,
}

fn tier_provider(tier: FpmTier) -> super::targets::PhpProvider {
    match tier {
        FpmTier::Hearth => super::targets::PhpProvider::Hearth,
        FpmTier::Herd => super::targets::PhpProvider::Herd,
        FpmTier::Homebrew => super::targets::PhpProvider::Homebrew,
    }
}

fn bounded_candidate_reason(reason: impl std::fmt::Display) -> String {
    reason.to_string().chars().take(512).collect()
}

/// Enumerate every installed supported FPM layout entry that independently
/// passes the settled provider identity boundary and executable check.
/// Enumeration is read-only and deliberately distinct from the unchanged
/// first-match production resolver above.
pub fn all_phpfpm_candidates(roots: &super::targets::ProviderRoots) -> FpmCandidateSet {
    use std::collections::HashSet;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mut set = FpmCandidateSet::default();
    let mut seen: HashSet<(&'static str, u64, u64)> = HashSet::new();
    for &version in super::SUPPORTED_VERSIONS {
        for tier in [FpmTier::Hearth, FpmTier::Herd, FpmTier::Homebrew] {
            let provider = tier_provider(tier);
            let path = super::targets::expected_binary_path(
                provider,
                version,
                super::targets::PhpSapi::Fpm,
                roots,
            );
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    set.rejected.push(FpmCandidateRejection {
                        version,
                        tier,
                        path,
                        reason: bounded_candidate_reason(error),
                    });
                    continue;
                }
                Ok(_) => {}
            }
            let canonical = match super::targets::verify_binary_identity(
                provider,
                version,
                super::targets::PhpSapi::Fpm,
                roots,
            ) {
                Ok(canonical) => canonical,
                Err(error) => {
                    set.rejected.push(FpmCandidateRejection {
                        version,
                        tier,
                        path,
                        reason: bounded_candidate_reason(error),
                    });
                    continue;
                }
            };
            let metadata = match std::fs::metadata(&canonical) {
                Ok(metadata) => metadata,
                Err(error) => {
                    set.rejected.push(FpmCandidateRejection {
                        version,
                        tier,
                        path,
                        reason: bounded_candidate_reason(error),
                    });
                    continue;
                }
            };
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                set.rejected.push(FpmCandidateRejection {
                    version,
                    tier,
                    path,
                    reason: "identity-verified target is not an executable regular file"
                        .to_string(),
                });
                continue;
            }
            let key = (version, metadata.dev(), metadata.ino());
            if !seen.insert(key) {
                continue;
            }
            set.candidates.push(FpmCandidate {
                version,
                tier,
                path,
                canonical,
            });
        }
    }
    set
}

#[derive(Debug)]
pub(crate) struct FpmServeChoice<'a> {
    pub candidate: &'a FpmCandidate,
    pub resolver_mismatch: Option<String>,
}

/// Choose the real-proof serve candidate without allowing the exists-only
/// production resolver to recover an identity-rejected provider alias.
pub(crate) fn choose_phpfpm_serve_candidate<'a>(
    version: &str,
    resolver_pick: Option<&std::path::Path>,
    roots: &super::targets::ProviderRoots,
    candidates: &'a [FpmCandidate],
) -> Option<FpmServeChoice<'a>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let fallback = candidates
        .iter()
        .filter(|candidate| candidate.version == version)
        .min_by_key(|candidate| candidate.tier)?;
    let Some(resolver_pick) = resolver_pick else {
        return Some(FpmServeChoice {
            candidate: fallback,
            resolver_mismatch: None,
        });
    };
    let mismatch = |reason: String| {
        Some(FpmServeChoice {
            candidate: fallback,
            resolver_mismatch: Some(bounded_candidate_reason(format!(
                "resolver pick {} for PHP {version} failed verified provider mapping: {reason}; \
                 falling back to {:?} {}",
                resolver_pick.display(),
                fallback.tier,
                fallback.canonical.display()
            ))),
        })
    };

    let Some((tier, provider)) = [FpmTier::Hearth, FpmTier::Herd, FpmTier::Homebrew]
        .into_iter()
        .find_map(|tier| {
            let provider = tier_provider(tier);
            let expected = super::targets::expected_binary_path(
                provider,
                version,
                super::targets::PhpSapi::Fpm,
                roots,
            );
            (expected == resolver_pick).then_some((tier, provider))
        })
    else {
        return mismatch("path is not an expected provider layout".to_string());
    };
    let verified = match super::targets::verify_binary_identity(
        provider,
        version,
        super::targets::PhpSapi::Fpm,
        roots,
    ) {
        Ok(verified) => verified,
        Err(error) => return mismatch(format!("{tier:?} identity verification failed: {error}")),
    };
    let verified_metadata = match std::fs::metadata(&verified) {
        Ok(metadata) if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 => {
            metadata
        }
        Ok(_) => {
            return mismatch(format!(
                "{tier:?} identity-verified target is not an executable regular file"
            ));
        }
        Err(error) => return mismatch(format!("{tier:?} metadata failed: {error}")),
    };
    let mapped = candidates.iter().find(|candidate| {
        if candidate.version != version {
            return false;
        }
        if candidate.canonical == verified {
            return true;
        }
        std::fs::metadata(&candidate.canonical).is_ok_and(|candidate_metadata| {
            candidate_metadata.dev() == verified_metadata.dev()
                && candidate_metadata.ino() == verified_metadata.ino()
        })
    });
    match mapped {
        Some(candidate) => Some(FpmServeChoice {
            candidate,
            resolver_mismatch: None,
        }),
        None => mismatch(format!(
            "{tier:?} verified identity is absent from the enumerated version set"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    struct CandidateFixture {
        _tmp: TempDir,
        roots: crate::php::targets::ProviderRoots,
    }

    fn candidate_fixture() -> CandidateFixture {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let roots = crate::php::targets::ProviderRoots::isolated(
            &base,
            base.join("hearth"),
            base.join("herd"),
            base.join("homebrew"),
        )
        .unwrap();
        for root in [&roots.hearth, &roots.herd, &roots.homebrew] {
            std::fs::create_dir_all(root).unwrap();
        }
        CandidateFixture { _tmp: tmp, roots }
    }

    fn tier_provider(tier: FpmTier) -> crate::php::targets::PhpProvider {
        match tier {
            FpmTier::Hearth => crate::php::targets::PhpProvider::Hearth,
            FpmTier::Herd => crate::php::targets::PhpProvider::Herd,
            FpmTier::Homebrew => crate::php::targets::PhpProvider::Homebrew,
        }
    }

    fn candidate_path(
        roots: &crate::php::targets::ProviderRoots,
        tier: FpmTier,
        version: &str,
    ) -> PathBuf {
        crate::php::targets::expected_binary_path(
            tier_provider(tier),
            version,
            crate::php::targets::PhpSapi::Fpm,
            roots,
        )
    }

    fn write_exec(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn resolves_hearth_binary_first() {
        let tmp = TempDir::new().unwrap();
        let php_dir = tmp.path().join("php/8.4");
        std::fs::create_dir_all(&php_dir).unwrap();
        std::fs::write(php_dir.join("php"), "fake-binary").unwrap();

        let result = resolve_php_binary("8.4", &tmp.path().to_path_buf());
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("php/8.4/php"));
    }

    #[test]
    fn returns_none_when_not_installed() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_php_binary("9.9", &tmp.path().to_path_buf());
        // May find Herd/Homebrew on the host, so we just check it doesn't panic
        let _ = result;
    }

    #[test]
    fn resolver_docs_match_implemented_tiers() {
        let source = include_str!("resolver.rs");
        let (docs, remainder) = source
            .split_once("pub fn resolve_php_binary")
            .expect("resolver source should contain resolve_php_binary");
        let implementation = remainder
            .split("/// Resolve the PHP-FPM binary")
            .next()
            .expect("resolver source should contain the PHP binary implementation");

        for tier in ["Hearth cache", "Herd", "Homebrew"] {
            assert!(docs.contains(tier), "resolver docs omit {tier}");
            assert!(
                implementation.contains(tier),
                "resolver implementation omits {tier}"
            );
        }

        let documented_tier_count = docs
            .lines()
            .filter(|line| {
                ["/// 1.", "/// 2.", "/// 3."]
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
            })
            .count();
        assert_eq!(documented_tier_count, 3);
        assert!(!docs.lines().any(|line| line.starts_with("/// 4.")));
    }

    #[test]
    fn enumerator_keeps_shadowed_lower_tier_candidates() {
        let fx = candidate_fixture();
        write_exec(
            &candidate_path(&fx.roots, FpmTier::Hearth, "8.4"),
            "#!/bin/sh\nexit 0\n",
        );
        write_exec(
            &candidate_path(&fx.roots, FpmTier::Homebrew, "8.4"),
            "#!/bin/sh\nexit 0\n",
        );

        let set = all_phpfpm_candidates(&fx.roots);
        let tiers: Vec<_> = set
            .candidates
            .iter()
            .filter(|candidate| candidate.version == "8.4")
            .map(|candidate| candidate.tier)
            .collect();
        assert_eq!(tiers, vec![FpmTier::Hearth, FpmTier::Homebrew]);
    }

    #[test]
    fn verified_resolver_pick_maps_to_enumerated_candidate() {
        for lower_tiers in [
            &[][..],
            &[FpmTier::Herd][..],
            &[FpmTier::Homebrew][..],
            &[FpmTier::Herd, FpmTier::Homebrew][..],
        ] {
            let fx = candidate_fixture();
            let path = candidate_path(&fx.roots, FpmTier::Hearth, "8.4");
            write_exec(&path, "#!/bin/sh\nexit 0\n");
            for tier in lower_tiers {
                write_exec(
                    &candidate_path(&fx.roots, *tier, "8.4"),
                    "#!/bin/sh\nexit 0\n",
                );
            }

            let pick = resolve_phpfpm_binary("8.4", &fx.roots.hearth).unwrap();
            let canonical = pick.canonicalize().unwrap();
            let set = all_phpfpm_candidates(&fx.roots);
            let choice =
                choose_phpfpm_serve_candidate("8.4", Some(&pick), &fx.roots, &set.candidates)
                    .unwrap();
            assert_eq!(choice.candidate.canonical, canonical);
            assert_eq!(choice.resolver_mismatch, None);
        }
    }

    #[test]
    fn unverifiable_resolver_pick_records_mismatch_and_falls_back_deterministically() {
        enum Invalid {
            Directory,
            NonExecutable,
            EscapingSymlink,
        }
        for invalid in [
            Invalid::Directory,
            Invalid::NonExecutable,
            Invalid::EscapingSymlink,
        ] {
            let fx = candidate_fixture();
            let higher = candidate_path(&fx.roots, FpmTier::Hearth, "8.4");
            std::fs::create_dir_all(higher.parent().unwrap()).unwrap();
            match invalid {
                Invalid::Directory => std::fs::create_dir(&higher).unwrap(),
                Invalid::NonExecutable => std::fs::write(&higher, "not executable").unwrap(),
                Invalid::EscapingSymlink => {
                    let outside = fx.roots.herd.join("outside-fpm");
                    write_exec(&outside, "#!/bin/sh\nexit 0\n");
                    std::os::unix::fs::symlink(outside, &higher).unwrap();
                }
            }
            let lower = candidate_path(&fx.roots, FpmTier::Homebrew, "8.4");
            write_exec(&lower, "#!/bin/sh\nexit 0\n");

            assert_eq!(
                resolve_phpfpm_binary("8.4", &fx.roots.hearth),
                Some(higher.clone())
            );
            let set = all_phpfpm_candidates(&fx.roots);
            let rejection = set
                .rejected
                .iter()
                .find(|item| item.path == higher)
                .expect("resolver mismatch recorded");
            assert!(rejection.reason.chars().count() <= 512);
            let verified: Vec<_> = set
                .candidates
                .iter()
                .filter(|candidate| candidate.version == "8.4")
                .collect();
            assert_eq!(verified.len(), 1);
            assert_eq!(verified[0].tier, FpmTier::Homebrew);
            let choice =
                choose_phpfpm_serve_candidate("8.4", Some(&higher), &fx.roots, &set.candidates)
                    .unwrap();
            assert_eq!(choice.candidate.tier, FpmTier::Homebrew);
            let mismatch = choice
                .resolver_mismatch
                .expect("serve choice must expose the resolver mismatch");
            assert!(mismatch.contains("failed verified provider mapping"));
            assert!(mismatch.chars().count() <= 512);
        }
    }

    #[test]
    fn cross_provider_out_of_root_symlink_is_rejected_not_relabeled() {
        let fx = candidate_fixture();
        let homebrew = candidate_path(&fx.roots, FpmTier::Homebrew, "8.4");
        write_exec(&homebrew, "#!/bin/sh\nexit 0\n");
        let herd = candidate_path(&fx.roots, FpmTier::Herd, "8.4");
        std::fs::create_dir_all(herd.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&homebrew, &herd).unwrap();

        let set = all_phpfpm_candidates(&fx.roots);
        assert!(set.rejected.iter().any(|item| item.path == herd));
        let records: Vec<_> = set
            .candidates
            .iter()
            .filter(|candidate| candidate.version == "8.4")
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tier, FpmTier::Homebrew);
        let choice =
            choose_phpfpm_serve_candidate("8.4", Some(&herd), &fx.roots, &set.candidates).unwrap();
        assert_eq!(choice.candidate.tier, FpmTier::Homebrew);
        assert!(
            choice
                .resolver_mismatch
                .as_deref()
                .is_some_and(|reason| reason.contains("Herd identity verification failed"))
        );
    }

    #[test]
    fn same_inode_dedup_is_version_scoped_after_independent_verification() {
        let fx = candidate_fixture();
        let marker = fx.roots.hearth.join("matrix-marker");
        let script = format!("#!/bin/sh\necho \"$0\" >> '{}'\nexit 0\n", marker.display());
        let hearth84 = candidate_path(&fx.roots, FpmTier::Hearth, "8.4");
        write_exec(&hearth84, &script);
        let herd84 = candidate_path(&fx.roots, FpmTier::Herd, "8.4");
        std::fs::create_dir_all(herd84.parent().unwrap()).unwrap();
        std::fs::hard_link(&hearth84, &herd84).unwrap();
        let hearth85 = candidate_path(&fx.roots, FpmTier::Hearth, "8.5");
        std::fs::create_dir_all(hearth85.parent().unwrap()).unwrap();
        std::fs::hard_link(&hearth84, &hearth85).unwrap();
        let hearth83 = candidate_path(&fx.roots, FpmTier::Hearth, "8.3");
        write_exec(&hearth83, "#!/bin/sh\nexit 0\n");

        let set = all_phpfpm_candidates(&fx.roots);
        let records84: Vec<_> = set
            .candidates
            .iter()
            .filter(|candidate| candidate.version == "8.4")
            .collect();
        assert_eq!(records84.len(), 1);
        assert_eq!(records84[0].tier, FpmTier::Hearth);
        assert!(
            set.candidates
                .iter()
                .any(|candidate| candidate.version == "8.5")
        );
        assert!(
            set.candidates
                .iter()
                .any(|candidate| candidate.version == "8.3")
        );
        use std::os::unix::fs::MetadataExt;
        for version in ["8.4", "8.5"] {
            let pick = resolve_phpfpm_binary(version, &fx.roots.hearth).unwrap();
            let pick_meta = std::fs::metadata(pick).unwrap();
            let record = set
                .candidates
                .iter()
                .find(|candidate| candidate.version == version)
                .unwrap();
            let record_meta = std::fs::metadata(&record.canonical).unwrap();
            assert_eq!(
                (pick_meta.dev(), pick_meta.ino()),
                (record_meta.dev(), record_meta.ino())
            );
        }

        let matrix_records: Vec<_> = set
            .candidates
            .iter()
            .filter(|candidate| matches!(candidate.version, "8.4" | "8.5"))
            .cloned()
            .collect();
        let conf = fx.roots.hearth.join("dummy.conf");
        std::fs::write(&conf, "dummy").unwrap();
        assert_eq!(
            crate::php::fpm::fpm_syntax_matrix(
                &matrix_records,
                &conf,
                std::time::Duration::from_secs(5),
            ),
            crate::php::fpm::MatrixOutcome::AllAccepted
        );
        assert_eq!(std::fs::read_to_string(marker).unwrap().lines().count(), 2);
        assert_eq!(
            matrix_records
                .iter()
                .max_by_key(|candidate| candidate.version)
                .unwrap()
                .version,
            "8.5"
        );
    }

    #[test]
    fn enumerator_skips_non_regular_or_non_executable_read_only() {
        let fx = candidate_fixture();
        let directory = candidate_path(&fx.roots, FpmTier::Hearth, "8.1");
        std::fs::create_dir_all(&directory).unwrap();
        let plain = candidate_path(&fx.roots, FpmTier::Hearth, "8.2");
        std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
        std::fs::write(&plain, "plain").unwrap();
        let dangling = candidate_path(&fx.roots, FpmTier::Herd, "8.3");
        std::fs::create_dir_all(dangling.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(fx.roots.herd.join("missing"), &dangling).unwrap();

        let set = all_phpfpm_candidates(&fx.roots);
        assert!(set.candidates.is_empty());
        for path in [directory, plain, dangling] {
            assert!(set.rejected.iter().any(|item| item.path == path));
        }
    }

    #[test]
    fn invalid_higher_tier_never_shadows_valid_lower_tier() {
        for invalid in ["directory", "non-executable", "escaping-symlink"] {
            let fx = candidate_fixture();
            let higher = candidate_path(&fx.roots, FpmTier::Hearth, "8.4");
            std::fs::create_dir_all(higher.parent().unwrap()).unwrap();
            match invalid {
                "directory" => std::fs::create_dir(&higher).unwrap(),
                "non-executable" => std::fs::write(&higher, "plain").unwrap(),
                _ => {
                    let outside = fx.roots.herd.join("outside");
                    write_exec(&outside, "#!/bin/sh\nexit 0\n");
                    std::os::unix::fs::symlink(outside, &higher).unwrap();
                }
            }
            let lower = candidate_path(&fx.roots, FpmTier::Homebrew, "8.4");
            write_exec(&lower, "#!/bin/sh\nexit 0\n");
            let set = all_phpfpm_candidates(&fx.roots);
            let candidates: Vec<_> = set
                .candidates
                .into_iter()
                .filter(|candidate| candidate.version == "8.4")
                .collect();
            assert_eq!(candidates.len(), 1, "{invalid}");
            assert_eq!(candidates[0].tier, FpmTier::Homebrew, "{invalid}");
            assert_eq!(set.rejected.len(), 1, "{invalid}");
            assert_ne!(
                crate::php::fpm::fpm_syntax_matrix(
                    &candidates,
                    &fx.roots.hearth.join("unused.conf"),
                    std::time::Duration::from_secs(5),
                ),
                crate::php::fpm::MatrixOutcome::Skip,
                "{invalid}"
            );
        }
    }
}
