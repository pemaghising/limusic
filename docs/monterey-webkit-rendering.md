# Monterey WebKit rendering (2026-10-31)

On Intel macOS 12.7.6, Safari reports 17.6 (17618.3.11.11.7), but the
system WebKit framework reports 17613.3.9.1.16. A standalone WKWebView
loading the generated production CSS reproduces the transparent background.

The background, foreground, opacity, positioning and stacking utilities are
present. The default WKWebView rejects `oklch(.145 .008 326)` and
`color-mix(in oklab, red 50%, transparent)`, but accepts percentage lightness.
Theme tokens with variable hues retain decimal lightness in the generated CSS.
These become invalid as colors, leaving the Now Playing panel transparent.
Tailwind's opacity utilities fall back to opaque colors without color-mix,
explaining solid pills and lost hierarchy. A `bg-foreground/10 text-foreground`
probe returns identical background/text colors on system WebKit; the updated
engine restores 10% background alpha. Position, inset and z-index compute
correctly. No evidence warrants changing NowPlaying, HomeHero or QueueList.

With the same probe and CSS, selecting Apple's installed staged Safari frameworks
via `DYLD_FRAMEWORK_PATH=/Library/Apple/System/Library/StagedFrameworks/Safari`
makes both feature checks pass. The panel background computes to
`oklch(0.145 0.008 326)`, foreground to `oklch(0.985 0 0)` and the 55% surface
to `oklab(0.145 0.0066322996 -0.004473545 / 0.55)`.

The fix in `src-tauri/build.rs` embeds the same LC_DYLD_ENVIRONMENT directives
as Apple's Safari executable: DYLD_VERSIONED_FRAMEWORK_PATH and
DYLD_VERSIONED_LIBRARY_PATH point at Apple's staged Safari directory. dyld uses
newer versions there and otherwise retains the system libraries. This applies
only to macOS binaries; it does not bundle a runtime or change the OS target.
A probe compiled with these directives passes all feature checks on normal
launch, including all 14 light/dark theme backgrounds. No CSS, theme, layout or
backend changes are necessary. The macOS 12.0 deployment target stays intact.

Remaining visual tests: normal launch of the rebuilt application;
Home and Now Playing Queue/Lyrics/Comments in light/dark and preset themes;
custom accents, artwork tint, readable pills, scrolling, transitions, popovers,
and video/theater transparency. Computed-style tests do not replace these.

Reproduce the computed-style comparison from the repository root:

```sh
clang -fobjc-arc -framework Cocoa -framework WebKit scripts/check-macos-webkit.m -o /tmp/limusic-webkit-check
css_file=$(rg -l tailwindcss ui/build/_app/immutable/assets/*.css)
/tmp/limusic-webkit-check "$css_file"
DYLD_FRAMEWORK_PATH=/Library/Apple/System/Library/StagedFrameworks/Safari /tmp/limusic-webkit-check "$css_file"
```

Verify the build fix independently (no environment override at launch):

```sh
clang -fobjc-arc -framework Cocoa -framework WebKit \
  -Wl,-dyld_env,DYLD_VERSIONED_FRAMEWORK_PATH=/Library/Apple/System/Library/StagedFrameworks/Safari \
  -Wl,-dyld_env,DYLD_VERSIONED_LIBRARY_PATH=/Library/Apple/System/Library/StagedFrameworks/Safari \
  scripts/check-macos-webkit.m -o /tmp/limusic-webkit-linked
/tmp/limusic-webkit-linked "$css_file"
```

Validation completed:
- `npm run check`: 0 errors, 0 warnings.
- `npm test`: 25/25 self-checks.
- Production build: passed (a concurrent earlier attempt failed from overlapping
  Svelte output writes; the final sequential packaging build completed).
- `rustfmt --check src-tauri/build.rs` and `git diff --check`: passed.
- Intel app rebuilt with MACOSX_DEPLOYMENT_TARGET=12.0 and
  LIBRARY_PATH=/opt/local/lib using Tauri CLI, unsigned, app bundle only.
- Bundle: target/release/bundle/macos/limusic.app.
- Mach-O: x86_64, minos 12.0; bundle LSMinimumSystemVersion: 12.0.
- Both LC_DYLD_ENVIRONMENT directives verified in the packaged executable.
- libmpv dependency verified as /opt/local/lib/libmpv.2.dylib.

Build warnings include an existing unused Volume field and future Rust
compatibility warning for block 0.1.6. rust-objcopy also cannot strip the
LC_DYLD_ENVIRONMENT load command (0x27); the build and packaging succeed with
that nonfatal warning and the directives remain intact. The bundle is unsigned
and intended for this local MacPorts installation, not redistribution.
