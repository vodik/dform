#!/bin/sh
# A release archive: `dist/dform-VERSION-TARGET.tar.gz`, one directory
# holding `dform` and every `dform-provider-*` beside it, which is where
# dform looks for a provider first (docs/reference.md "Installing").
# The release workflow runs it per target; run it locally for the host's.
#
#   .github/dist.sh VERSION TARGET
set -eu
version=$1
target=$2

# Every provider executable the workspace builds.
providers=$(cargo metadata --format-version 1 --no-deps |
    jq -r '.packages[].targets[] | select(.kind == ["bin"]) | .name |
           select(startswith("dform-provider-"))' | sort)

bins="--bin dform"
for p in $providers; do
    bins="$bins --bin $p"
done

# The host's own build in target/release; another target's under its name.
if [ "$target" = "$(rustc -vV | sed -n 's/^host: //p')" ]; then
    out=target/release
    # shellcheck disable=SC2086
    cargo build --release --locked --workspace $bins
else
    out=target/$target/release
    # shellcheck disable=SC2086
    cargo build --release --locked --workspace --target "$target" $bins
fi

name=dform-$version-$target
rm -rf "dist/$name" "dist/$name.tar.gz"
mkdir -p "dist/$name"
for b in dform $providers; do
    cp "$out/$b" "dist/$name/"
done
cp LICENSE README.md "dist/$name/"
tar -C dist -czf "dist/$name.tar.gz" "$name"
rm -r "dist/$name"
echo "dist/$name.tar.gz"
