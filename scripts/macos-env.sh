# Source from the checkout root before invoking native compilers outside Cargo.
# Linux callers share the same probe scripts and have no macOS target.
if [ "$(uname -s)" = Darwin ]; then
    MACOSX_DEPLOYMENT_TARGET=$(sed -nE \
        's/^MACOSX_DEPLOYMENT_TARGET = \{ value = "([0-9]+\.[0-9]+(\.[0-9]+)?)", force = true \}$/\1/p' \
        .cargo/config.toml)
    case "$MACOSX_DEPLOYMENT_TARGET" in
        ''|*[!0-9.]*)
            echo 'Expected one forced macOS version entry in .cargo/config.toml' >&2
            return 1
            ;;
    esac
    export MACOSX_DEPLOYMENT_TARGET
fi
