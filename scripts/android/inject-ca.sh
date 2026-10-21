#!/system/bin/sh
# Inject a CA certificate into the Android system trust store, until the next
# reboot. Run as root: `sh inject-ca.sh /data/local/tmp/<subject_hash_old>.0`.
#
# The approach follows HTTP Toolkit's (github.com/httptoolkit/httptoolkit-server,
# src/interceptors/android/adb-commands.ts): shadow the read-only store with a
# tmpfs holding a copy of the original certs plus ours, and on Android 14+ bind
# that tmpfs over the Conscrypt APEX store inside the mount namespace of the
# zygotes and of every running app.
#
# Safe to run again: existing mounts are reused rather than stacked, and the
# cert keeps its fixed `<hash>.0` name, so it is replaced rather than duplicated.
#
# Only uses mksh and toybox features shipped with Android.

set -eu

TAG=sieve-inject-ca
SYSTEM_STORE=/system/etc/security/cacerts
APEX_STORE=/apex/com.android.conscrypt/cacerts
STAGING=/data/local/tmp/sieve-cacerts-copy
LABEL=u:object_r:system_file:s0

log() {
    echo "$TAG: $*"
}

die() {
    echo "$TAG: FAILED: $*" >&2
    exit 1
}

# True if the mounts table in $1 has a tmpfs mounted exactly at $2.
has_tmpfs() {
    grep -q " $2 tmpfs " "$1" 2>/dev/null
}

[ $# -eq 1 ] || die "usage: $0 <path to <subject_hash_old>.0>"
CERT=$1
NAME=${CERT##*/}
case $NAME in
    *.0) ;;
    *) die "$NAME is not named <subject_hash_old>.0" ;;
esac
case ${NAME%.0} in
    '' | *[!0-9a-f]*) die "$NAME is not named <subject_hash_old>.0" ;;
esac
[ -f "$CERT" ] || die "$CERT does not exist"
[ "$(id -u)" = 0 ] || die "must run as root (uid is $(id -u))"

# Mounts made in a private namespace (e.g. a Magisk su session) would stay
# invisible to the rest of the system, so move to init's namespace first.
if [ -z "${SIEVE_IN_INIT_NS:-}" ] && command -v nsenter >/dev/null &&
    [ "$(readlink /proc/1/ns/mnt)" != "$(readlink /proc/$$/ns/mnt)" ]; then
    log "re-running in init's mount namespace"
    export SIEVE_IN_INIT_NS=1
    exec nsenter --mount=/proc/1/ns/mnt -- sh "$0" "$@"
fi

SDK=$(getprop ro.build.version.sdk)
log "Android API level ${SDK:-unknown}"

# Android 14+ reads CAs from the Conscrypt APEX instead of /system.
USE_APEX=
[ -d "$APEX_STORE" ] && USE_APEX=1

# 1. Make $SYSTEM_STORE a writable tmpfs seeded with the current certs. On
#    Android 14+ it also serves as the source of the APEX bind mounts: unlike
#    /apex, mounts under /system propagate to the zygotes' namespaces.
if has_tmpfs /proc/$$/mounts "$SYSTEM_STORE"; then
    log "$SYSTEM_STORE is already a tmpfs, reusing it"
else
    rm -rf "$STAGING"
    mkdir -m 700 "$STAGING"
    cp "$SYSTEM_STORE"/* "$STAGING"/
    # The APEX set is the one Android 14+ actually trusts, so it wins on
    # name clashes.
    if [ -n "$USE_APEX" ]; then
        cp "$APEX_STORE"/* "$STAGING"/
    fi
    mount -t tmpfs sieve-cacerts "$SYSTEM_STORE"
    cp "$STAGING"/* "$SYSTEM_STORE"/
    rm -rf "$STAGING"
    log "mounted a tmpfs over $SYSTEM_STORE"
fi

# 2. Add our cert, then fix ownership, mode, and SELinux label on everything.
cp "$CERT" "$SYSTEM_STORE/$NAME"
chown root:root "$SYSTEM_STORE" "$SYSTEM_STORE"/*
chmod 755 "$SYSTEM_STORE"
chmod 644 "$SYSTEM_STORE"/*
chcon "$LABEL" "$SYSTEM_STORE" "$SYSTEM_STORE"/*
[ -f "$SYSTEM_STORE/$NAME" ] || die "$NAME is missing from $SYSTEM_STORE"

if [ -z "$USE_APEX" ]; then
    log "OK: $NAME is in $SYSTEM_STORE (API ${SDK:-unknown}); undone by reboot"
    exit 0
fi

# 3. Android 14+: APEX mounts are per namespace, so bind the tmpfs over the
#    APEX store in each one that matters. $1 is a pid, $2 is "required" if a
#    failure should abort (zygotes) rather than be counted (apps that may have
#    exited meanwhile). Namespaces that already have it are left alone.
bind_apex() {
    if has_tmpfs "/proc/$1/mounts" "$APEX_STORE"; then
        return 0
    fi
    # The source path resolves inside the target namespace, so check that the
    # tmpfs propagated there before binding it.
    # shellcheck disable=SC2016 # expanded by the inner sh
    if nsenter --mount="/proc/$1/ns/mnt" -- sh -c \
        '[ -f "$1/$3" ] && mount --bind "$1" "$2"' \
        sh "$SYSTEM_STORE" "$APEX_STORE" "$NAME" 2>/dev/null; then
        BOUND=$((BOUND + 1))
    elif [ "${2:-}" = required ]; then
        die "could not bind $SYSTEM_STORE over $APEX_STORE for pid $1"
    else
        SKIPPED=$((SKIPPED + 1))
    fi
}

BOUND=0
SKIPPED=0

# Our own namespace first, so the result can be checked from `adb shell`.
if ! has_tmpfs /proc/$$/mounts "$APEX_STORE"; then
    mount --bind "$SYSTEM_STORE" "$APEX_STORE"
fi

# Apps inherit the zygote's mounts when they fork, so new apps see the store.
ZYGOTES="$(pidof zygote || true) $(pidof zygote64 || true)"
case $ZYGOTES in
    *[0-9]*) ;;
    *) die "no zygote process found" ;;
esac
for pid in $ZYGOTES; do
    bind_apex "$pid" required
done

# Apps that are already running need it in their own namespace too.
for zygote in $ZYGOTES; do
    for pid in $(pgrep -P "$zygote" || true); do
        bind_apex "$pid"
    done
done
log "bound the store into $BOUND namespace(s); $SKIPPED app(s) exited or failed"

# 4. Verify from our namespace and from every zygote's.
[ -f "$APEX_STORE/$NAME" ] || die "$NAME is missing from $APEX_STORE"
for pid in $ZYGOTES; do
    # shellcheck disable=SC2016 # expanded by the inner sh
    nsenter --mount="/proc/$pid/ns/mnt" -- sh -c '[ -f "$1" ]' sh "$APEX_STORE/$NAME" ||
        die "$NAME is not visible to zygote pid $pid"
done

log "OK: $NAME is in $SYSTEM_STORE and $APEX_STORE (API ${SDK:-unknown}); undone by reboot"
