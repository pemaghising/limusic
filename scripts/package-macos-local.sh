#!/bin/bash
# Package an existing Intel build using dylibbundler; never modify the source .app.
set -euo pipefail
root_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
source_app=${1:-"$root_dir/target/release/bundle/macos/limusic.app"}
output_dir=${2:-"$root_dir/target/portable-macos"}
bundler=${DYLIBBUNDLER:-dylibbundler}
command -v "$bundler" >/dev/null || { echo 'Install dylibbundler or set DYLIBBUNDLER to its executable.' >&2; exit 1; }
app="$output_dir/limusic.app"
[ ! -e "$app" ] || { echo "Output exists: $app. Choose a fresh output directory." >&2; exit 1; }
mkdir -p "$output_dir"
ditto "$source_app" "$app"
executable=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app/Contents/Info.plist")
"$bundler" -cd -of -b -ns -x "$app/Contents/MacOS/$executable" \
    -d "$app/Contents/Frameworks" -p '@executable_path/../Frameworks' \
    -s /opt/local/lib < /dev/null
python3 "$root_dir/scripts/check-macos-bundle.py" --dedupe "$app"
mkdir -p "$app/Contents/Resources/Licenses"
cp "$root_dir/LICENSE" "$app/Contents/Resources/Licenses/LiMusic-GPL-3.0.txt"
find "$app/Contents/Frameworks" -type f -name '*.dylib' -exec basename {} \; | sort > "$app/Contents/Resources/BUNDLED-LIBRARIES.txt"
# Ad-hoc signing is for local builds; Developer ID/notarization is a separate release step.
while IFS= read -r -d '' library; do
    codesign --force --sign - "$library"
done < <(find "$app/Contents/Frameworks" -type f -name '*.dylib' -print0)
codesign --force --sign - "$app"
codesign --verify --deep --strict --verbose=2 "$app"
python3 "$root_dir/scripts/check-macos-bundle.py" "$app"
ditto -c -k --sequesterRsrc --keepParent "$app" "$output_dir/limusic-monterey-intel.zip"
shasum -a 256 "$output_dir/limusic-monterey-intel.zip" > "$output_dir/SHA256SUMS"
echo "Packaged: $app"
echo "Archive: $output_dir/limusic-monterey-intel.zip"
