#!/usr/bin/env sh
set -eu

project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
output_input="${2:-dist}"
case "$output_input" in
  /*) output="$output_input" ;;
  *) output="$project/$output_input" ;;
esac
version=$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"harness-cli","version":"\([^"]*\)".*/\1/p' | head -n 1)
package_name="kernary-code-$version-$target"
package="$output/$package_name"
archive="$output/$package_name.tar.gz"
if [ -e "$package" ] || [ -e "$archive" ]; then
  echo "发布目标已存在，拒绝覆盖：$package_name" >&2
  exit 1
fi
mkdir -p "$package/bin" "$package/completions" "$package/man" "$package/assets" "$package/examples"
cargo build --locked --release -p harness-cli --bins --target "$target"
binary="$project/target/$target/release/kernary"
test -x "$binary"
cp "$binary" "$package/bin/kernary"
cp "$binary" "$package/bin/harness"
cp "$project/LICENSE-APACHE" "$project/release/README_ZH.md" "$project/release/install.ps1" "$project/release/install.sh" "$package/"
cp "$project/Cargo.lock" "$package/DEPENDENCIES.lock"
cp "$project/assets/kernary-kern.svg" "$package/assets/"
cp "$project/kernary.providers.example.toml" "$package/examples/kernary.providers.toml"
cp "$project/kernary.lsp.example.toml" "$package/examples/kernary.lsp.toml"
cp "$project/kernary.example.toml" "$package/examples/kernary.toml"
cp "$project/kernary.mcp.example.toml" "$package/examples/kernary.mcp.toml"
cp "$project/kernary.permissions.example.toml" "$package/examples/kernary.permissions.toml"
chmod 0755 "$package/bin/kernary" "$package/bin/harness" "$package/install.sh"
"$package/bin/kernary" completions bash > "$package/completions/kernary.bash"
"$package/bin/kernary" completions zsh > "$package/completions/_kernary"
"$package/bin/kernary" completions fish > "$package/completions/kernary.fish"
"$package/bin/kernary" completions powershell > "$package/completions/_kernary.ps1"
"$package/bin/kernary" completions elvish > "$package/completions/kernary.elv"
"$package/bin/kernary" man > "$package/man/kernary.1"
"$package/bin/harness" completions bash > "$package/completions/harness.bash"
"$package/bin/harness" completions zsh > "$package/completions/_harness"
"$package/bin/harness" completions fish > "$package/completions/harness.fish"
"$package/bin/harness" completions powershell > "$package/completions/_harness.ps1"
"$package/bin/harness" completions elvish > "$package/completions/harness.elv"
"$package/bin/harness" man > "$package/man/harness.1"
if command -v sha256sum >/dev/null 2>&1; then
  binary_hash=$(sha256sum "$package/bin/kernary" | awk '{print $1}')
  compatibility_hash=$(sha256sum "$package/bin/harness" | awk '{print $1}')
else
  binary_hash=$(shasum -a 256 "$package/bin/kernary" | awk '{print $1}')
  compatibility_hash=$(shasum -a 256 "$package/bin/harness" | awk '{print $1}')
fi
binary_bytes=$(wc -c < "$package/bin/kernary" | tr -d ' ')
if command -v sha256sum >/dev/null 2>&1; then
  lock_hash=$(sha256sum "$package/DEPENDENCIES.lock" | awk '{print $1}')
else
  lock_hash=$(shasum -a 256 "$package/DEPENDENCIES.lock" | awk '{print $1}')
fi
printf '{\n  "schemaVersion": 2,\n  "name": "kernary-code",\n  "primaryCommand": "kernary",\n  "compatibilityCommands": ["harness"],\n  "compatibilityCommand": "harness",\n  "version": "%s",\n  "target": "%s",\n  "primaryBinary": "bin/kernary",\n  "compatibilityBinaries": ["bin/harness"],\n  "binary": "bin/kernary",\n  "binaryBytes": %s,\n  "binarySha256": "%s",\n  "compatibilityBinarySha256": "%s",\n  "stateDirectory": ".harness",\n  "credentialService": "dev.openai.harness",\n  "environmentPrefixes": ["KERNARY_", "HARNESS_"],\n  "dependencyLockSha256": "%s"\n}\n' \
  "$version" "$target" "$binary_bytes" "$binary_hash" "$compatibility_hash" "$lock_hash" > "$package/release-manifest.json"
tar -C "$output" -czf "$archive" "$package_name"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$output" && sha256sum "$(basename "$archive")") > "$archive.sha256"
else
  (cd "$output" && shasum -a 256 "$(basename "$archive")") > "$archive.sha256"
fi
printf '%s\n' "$archive"
