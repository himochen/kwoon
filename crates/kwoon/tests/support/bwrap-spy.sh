#!/bin/sh
# Stands in for bubblewrap where a test must see or change what kwoon asks of it. Host
# settings name it as `paths.bwrap`, so kwoon reaches it through its production code.
#
# It appends each invocation's arguments and environment to `log` next to it, then
# runs the real bubblewrap, logging the arguments it runs it with. kwoon starts it
# with an empty environment, so the behaviour comes from the file `mode` next to it:
#   (absent)            run bubblewrap unchanged
#   report <file>       for the full-layout probe (stage 2), print <file> instead
#                       (only in a test context, as for `writable`)
#   no-cgroup           do not ask for a cgroup namespace
#   writable <dir>      for stage 2, bind <dir> writable instead of read-only, and edit
#                       the probe request so that it declares the bind writable too
#   add-writable <src> <dest>
#                       for stage 2, add a writable bind of <src> at <dest>, right after
#                       bubblewrap's own /tmp, and edit the probe request so that it
#                       declares <dest> writable
#   swap-home <target>  after the harness (stage 3) exits, replace the run home with a
#                       symbolic link to <target>, as a host process could
set -u
here=$(dirname "$0")
log="$here/log"

# kwoon passes bubblewrap's options through `--args <fd>`, NUL-separated, and the
# command on argv. Rebuild the whole list as options then command, so that the modes
# below see what bubblewrap would; the real bubblewrap then gets it on argv, which is
# fine for this test-only stand-in.
if [ "${1:-}" = --args ]; then
    fd=$2
    shift 2
    options="$here/options.$$"
    eval "/usr/bin/cat <&$fd" > "$options"
    # Close it, as bubblewrap does once it has read it: nothing else may inherit it.
    eval "exec $fd<&-"
    # The options are rebuilt one per line below, so an option holding a newline would
    # become two, which bubblewrap never does. Refuse it rather than mirror it wrongly.
    if [ "$(/usr/bin/tr -dc '\n' < "$options" | /usr/bin/wc -c)" -ne 0 ]; then
        /usr/bin/rm -f "$options"
        echo "bwrap-spy: an option contains a newline; refusing to split it" >&2
        exit 1
    fi
    /usr/bin/tr '\0' '\n' < "$options" > "$options.lines"
    /usr/bin/mv "$options.lines" "$options"
    count=$#
    while IFS= read -r option; do set -- "$@" "$option"; done < "$options"
    /usr/bin/rm -f "$options"
    # Move the command, now first, behind the options.
    i=0
    while [ "$i" -lt "$count" ]; do
        arg=$1
        shift
        set -- "$@" "$arg"
        i=$((i + 1))
    done
fi
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

# The modes that change stage 2's evidence refuse unless kwoon runs in an isolated
# verification context: the state directory named in the probe request is classified
# "test". A normal context that names this script as its bubblewrap gets no
# substituted evidence.
in_test_context() {
    request=""
    previous=""
    for arg do
        if [ "$arg" = /.kwoon-probe-request ] && [ -z "$request" ]; then request="$previous"; fi
        previous="$arg"
    done
    [ -n "$request" ] || return 1
    state=$(/usr/bin/sed -n 's/.*"state_dir":"\([^"]*\)".*/\1/p' "$request")
    [ -n "$state" ] && [ "$(/usr/bin/cat "$state/classification" 2>/dev/null)" = test ]
}

if [ "$stage" = 2 ] && { [ "$mode" = report ] || [ "$mode" = writable ] || [ "$mode" = add-writable ]; }; then
    if ! in_test_context "$@"; then
        echo "bwrap-spy: refusing to change stage 2 outside a test context" >&2
        exit 1
    fi
fi

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
add-writable)
    if [ "$stage" = 2 ]; then
        source=${target%% *}
        dest=${target#* }
        # Right after bubblewrap's own /tmp, before the binds made beneath it.
        request=""
        previous=""
        for arg do
            shift
            if [ "$arg" = /.kwoon-probe-request ] && [ -z "$request" ]; then request="$previous"; fi
            set -- "$@" "$arg"
            if [ "$previous:$arg" = --tmpfs:/tmp ]; then set -- "$@" --bind "$source" "$dest"; fi
            previous="$arg"
        done
        /usr/bin/sed -i "s|\"mounts\":\[|\"mounts\":[{\"mount_point\":\"$dest\",\"access\":\"writable\",\"optional\":false},|" "$request"
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
