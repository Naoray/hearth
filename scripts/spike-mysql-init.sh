#!/usr/bin/env bash
# Day-1 spike for Hearth v0.3.0 Block D (MySQL/MariaDB).
#
# Probes whichever `mysqld` is on disk (Homebrew + Herd) to learn which init
# flow applies, then runs that init in a throwaway tempdir to confirm Block D
# can rely on the branch.
#
# Outcome is captured under scripts/spike-results/ — Block D reads it.
set -euo pipefail

RESULT_DIR="$(cd "$(dirname "$0")" && pwd)/spike-results"
mkdir -p "$RESULT_DIR"
REPORT="$RESULT_DIR/mysql-init.log"

CANDIDATES=(
    "$HOME/Library/Application Support/Herd/bin/mysqld"
    "$HOME/Library/Application Support/Herd/bin/mariadbd"
    "/opt/homebrew/opt/mysql/bin/mysqld"
    "/opt/homebrew/opt/mariadb/bin/mariadbd"
    "/opt/homebrew/opt/mariadb/bin/mysqld"
    "/usr/local/opt/mysql/bin/mysqld"
    "/usr/local/opt/mariadb/bin/mariadbd"
    "/usr/local/opt/mariadb/bin/mysqld"
)

probe_one() {
    local bin="$1"
    [ -x "$bin" ] || return 1

    echo "=== $bin ===" | tee -a "$REPORT"
    local version
    version=$("$bin" --version 2>&1 || true)
    echo "version: $version" | tee -a "$REPORT"

    local flavor="unknown"
    if echo "$version" | grep -qi 'mariadb'; then
        flavor="mariadb"
    elif echo "$version" | grep -qiE 'mysql\b|Ver [0-9]+\.[0-9]+\.[0-9]+ for'; then
        flavor="mysql"
    fi
    echo "flavor: $flavor" | tee -a "$REPORT"

    local tmpdata
    tmpdata=$(mktemp -d /tmp/hearth-spike-mysqld.XXXXXX)
    echo "tmpdata: $tmpdata" | tee -a "$REPORT"

    case "$flavor" in
        mariadb)
            # MariaDB ships mariadb-install-db; on some builds mysqld also
            # accepts --initialize-insecure. Try the install-db form first.
            local mariadb_install
            mariadb_install="$(dirname "$bin")/mariadb-install-db"
            if [ -x "$mariadb_install" ]; then
                echo "trying: $mariadb_install --datadir=$tmpdata --auth-root-authentication-method=normal" | tee -a "$REPORT"
                if "$mariadb_install" --datadir="$tmpdata" --auth-root-authentication-method=normal >>"$REPORT" 2>&1; then
                    echo "result: OK (mariadb-install-db)" | tee -a "$REPORT"
                else
                    echo "result: FAILED (mariadb-install-db)" | tee -a "$REPORT"
                fi
            else
                echo "trying: $bin --initialize-insecure --datadir=$tmpdata" | tee -a "$REPORT"
                if "$bin" --initialize-insecure --datadir="$tmpdata" >>"$REPORT" 2>&1; then
                    echo "result: OK (mysqld --initialize-insecure on mariadb)" | tee -a "$REPORT"
                else
                    echo "result: FAILED" | tee -a "$REPORT"
                fi
            fi
            ;;
        mysql)
            echo "trying: $bin --initialize-insecure --datadir=$tmpdata" | tee -a "$REPORT"
            if "$bin" --initialize-insecure --datadir="$tmpdata" >>"$REPORT" 2>&1; then
                echo "result: OK (mysqld --initialize-insecure)" | tee -a "$REPORT"
            else
                echo "result: FAILED" | tee -a "$REPORT"
            fi
            ;;
        *)
            echo "result: SKIPPED (unknown flavor)" | tee -a "$REPORT"
            ;;
    esac

    ls "$tmpdata" | head -10 | sed 's/^/  /' | tee -a "$REPORT"
    rm -rf "$tmpdata"
    echo "" | tee -a "$REPORT"
}

: > "$REPORT"
echo "spike run: $(date -u +%FT%TZ)" | tee -a "$REPORT"
echo "host: $(uname -a)" | tee -a "$REPORT"
echo "" | tee -a "$REPORT"

found=0
for bin in "${CANDIDATES[@]}"; do
    if [ -e "$bin" ]; then
        probe_one "$bin" || true
        found=$((found + 1))
    fi
done

if [ $found -eq 0 ]; then
    echo "no mysqld found in candidates" | tee -a "$REPORT"
    exit 0
fi

echo "spike done. report: $REPORT"
