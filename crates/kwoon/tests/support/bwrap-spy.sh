#!/bin/sh
# Stands in for bubblewrap where a test must see or change what kwoon asks of it. Host
# settings name it as `paths.bwrap`, so kwoon reaches it through its production code.
#
# It appends each invocation's arguments and environment to `log` next to it, then
# runs the real bubblewrap, logging the arguments it runs it with. kwoon starts it with an empty environment, so the
# behaviour comes from the file `mode` next to it:
#   (absent)            run bubblewrap unchanged
#   report <file>       for the full-layout probe (stage 2), print <file> instead
#   no-cgroup           do not ask for a cgroup namespace
#   writable <dir>      for stage 2, bind <dir> writable instead of read-only, and edit
#                       the probe request so that it declares the bind writable too
#   swap-home <target>  after the harness (stage 3) exits, replace the run home with a
#                       symbolic link to <target>, as a host process could
set -u
here=$(dirname "$0")
log="$here/log"
{
    printf 'argv'
    for arg do printf ' [%s]' "$arg"; done
    printf '\nenv\n'
    /usr/bin/env
    printf 'end\n'
} >> "$log"

mode=""
target=""
if [ -r "$here/mode" ]; then
    read -r mode target < "$here/mode" || true
fi

stage=3
case " $* " in
*" __probe "*)
    case " $* " in
    *" --proc "*) stage=2 ;;
    *) stage=1 ;;
    esac
    ;;
esac

case "$mode" in
report)
    if [ "$stage" = 2 ]; then
        /usr/bin/cat "$target"
        exit 0
    fi
    ;;
writable)
    if [ "$stage" = 2 ]; then
        held=""
        request=""
        previous=""
        for arg do
            shift
            if [ -n "$held" ]; then
                # A held --ro-bind: --bind if it binds the target.
                if [ "$arg" = "$target" ]; then set -- "$@" --bind; else set -- "$@" --ro-bind; fi
                held=""
            fi
            if [ "$arg" = --ro-bind ]; then
                held=1
            else
                set -- "$@" "$arg"
            fi
            if [ "$arg" = /.kwoon-probe-request ] && [ -z "$request" ]; then request="$previous"; fi
            previous="$arg"
        done
        /usr/bin/sed -i "s|{\"mount_point\":\"$target\",\"access\":\"read-only\"|{\"mount_point\":\"$target\",\"access\":\"writable\"|" "$request"
    fi
    ;;
no-cgroup)
    for arg do
        shift
        case "$arg" in
        --unshare-all) set -- "$@" --unshare-user --unshare-ipc --unshare-pid --unshare-uts ;;
        --share-net) ;;
        *) set -- "$@" "$arg" ;;
        esac
    done
    ;;
swap-home)
    if [ "$stage" = 3 ]; then
        home=""
        previous=""
        for arg do
            case "$previous:$arg" in
            --bind:*/home) home="$arg" ;;
            esac
            previous="$arg"
        done
        /usr/bin/bwrap "$@"
        status=$?
        /usr/bin/mv "$home" "$home.real" && /usr/bin/ln -s "$target" "$home"
        exit "$status"
    fi
    ;;
esac
{
    printf 'exec'
    for arg do printf ' [%s]' "$arg"; done
    printf '\n'
} >> "$log"
exec /usr/bin/bwrap "$@"
