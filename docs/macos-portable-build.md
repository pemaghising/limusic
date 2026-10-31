# Local Intel Monterey application bundle

This fork retains LiMusic's upstream history and GPL-3.0 license. Local changes
add macOS 12.0 support, select Apple's installed updated Safari frameworks, and
make Familiar Artists a compact scrolling row.

An ordinary Tauri build still links to MacPorts. To make a local app with those
libraries inside it, build first, then package:

```sh
# Run from the repository root. Node, Rust and Apple development tools required.
MACOSX_DEPLOYMENT_TARGET=12.0 LIBRARY_PATH=/opt/local/lib \
  npx --yes @tauri-apps/cli@2 build --bundles app --no-sign \
  --config '{"build":{"beforeBuildCommand":"npm run build"},"bundle":{"createUpdaterArtifacts":false}}'
# Install dylibbundler with MacPorts, or compile it separately and set DYLIBBUNDLER.
scripts/package-macos-local.sh
```

The script copies the app into `target/portable-macos/limusic.app`, bundles the
MacPorts dylib dependency graph, checks paths/Intel slices, removes duplicate
rpaths, ad-hoc signs it, verifies signatures, and creates a ZIP and SHA-256 file.
It refuses to overwrite an existing output app; pass a fresh output directory
as its second argument to repeat packaging. The original build is preserved.
Python 3.9+, `dylibbundler`, and Apple command-line tools are required.

For example, with a separately compiled tool:

```sh
DYLIBBUNDLER=/path/to/dylibbundler scripts/package-macos-local.sh
open target/portable-macos/limusic.app
```

This is an Intel, locally ad-hoc-signed build. It still relies on macOS system
frameworks, including an installed Safari/WebKit with the required color support.
It is not notarized. Full playback and UI testing on a Mac without MacPorts is
still required; link-path checks cannot discover every runtime-loaded plugin,
font configuration or external data dependency.

No binaries are published to GitHub by these scripts. Before distributing a
binary publicly, complete signing/notarization as appropriate and supply the
required third-party notices and corresponding sources/build instructions for
bundled GPL/LGPL dependencies as well as LiMusic. A library inventory and the
LiMusic license are included in this local bundle; they are not a complete
third-party license/source distribution package.
