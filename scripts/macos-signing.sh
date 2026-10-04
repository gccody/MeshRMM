# Code signing shared by the macOS build scripts. Source it; don't run it.
#
# MESHRMM_CODESIGN_IDENTITY selects the signing identity. Without it, the
# keychain's only Developer ID Application certificate signs the bundle.
# Set it to "-" for an ad-hoc development signature, which macOS doesn't tie
# to a developer team: privacy permissions reset on every build, and installed
# Agents signed by a team refuse to update to it.

meshrmm_codesign_identity() {
    if [ -n "${MESHRMM_CODESIGN_IDENTITY:-}" ]; then
        printf '%s\n' "$MESHRMM_CODESIGN_IDENTITY"
        return
    fi
    identities=$(security find-identity -v -p codesigning | sed -n 's/^ *[0-9][0-9]*) \([0-9A-F]\{40\}\) "\(Developer ID Application: .*\)"$/\1 \2/p')
    count=$(printf '%s' "$identities" | grep -c . || true)
    if [ "$count" -eq 1 ]; then
        printf '%s\n' "${identities%% *}"
        return
    fi
    if [ "$count" -eq 0 ]; then
        echo "No Developer ID Application certificate is in the keychain." >&2
    else
        echo "Several Developer ID Application certificates are in the keychain:" >&2
        printf '%s\n' "$identities" >&2
    fi
    echo "Set MESHRMM_CODESIGN_IDENTITY to a certificate's SHA-1 hash, or to - for an ad-hoc development signature." >&2
    return 1
}

# Signs an app bundle. Extra arguments, such as --deep, are passed to codesign.
meshrmm_sign_app() {
    identity=$1
    app=$2
    shift 2
    if [ "$identity" = "-" ]; then
        codesign --force "$@" --sign - "$app"
    else
        codesign --force "$@" --options runtime --timestamp --sign "$identity" "$app"
    fi
    codesign --verify --strict --deep "$app"
}
