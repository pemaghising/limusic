// Compare computed production styles under system and updated Safari WKWebView.
#import <Cocoa/Cocoa.h>
#import <WebKit/WebKit.h>

@interface Probe : NSObject <WKNavigationDelegate>
@property WKWebView *web;
@end

@implementation Probe
- (void)webView:(WKWebView *)web didFinishNavigation:(WKNavigation *)navigation {
    NSString *script =
        @"JSON.stringify({"
        "supports: ['oklch(.145 .008 326)', 'oklch(14.5% .008 326)',"
        "'color-mix(in oklab, red 50%, transparent)'].map(v => [v, CSS.supports('color', v)]),"
        "items: [...document.querySelectorAll('[data-test]')].map(e => {"
        "const s = getComputedStyle(e); return {test: e.dataset.test, bg: s.backgroundColor,"
        "color: s.color, position: s.position, inset: s.inset, z: s.zIndex}}),"
        "themes: ['default','catppuccin','tokyonight','caffeine','neon','breeze','amoled']"
        ".flatMap(t => [false,true].map(d => {"
        "document.documentElement.className = 'theme-' + t + (d ? ' dark' : '');"
        "return {theme: t, dark: d, bg: getComputedStyle("
        "document.querySelector('[data-test=panel]')).backgroundColor}}))})";
    [web evaluateJavaScript:script completionHandler:^(id result, NSError *error) {
        NSLog(@"RESULT %@ %@", result, error);
        [NSApp terminate:nil];
    }];
}
@end

int main(int argc, const char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: check-macos-webkit path/to/production.css\n");
        return 2;
    }
    @autoreleasepool {
        NSError *error = nil;
        NSString *css = [NSString stringWithContentsOfFile:[NSString stringWithUTF8String:argv[1]]
                                                encoding:NSUTF8StringEncoding error:&error];
        if (!css) {
            NSLog(@"Cannot read stylesheet: %@", error);
            return 1;
        }
        [NSApplication sharedApplication];
        Probe *probe = [Probe new];
        probe.web = [[WKWebView alloc] initWithFrame:NSMakeRect(0, 0, 1200, 800)];
        probe.web.navigationDelegate = probe;
        NSString *html = [NSString stringWithFormat:
            @"<html class='dark'><style>%@</style><body>"
            "<div data-test='panel' class='absolute inset-y-0 left-16 right-0 z-20 bg-background text-foreground'>Panel</div>"
            "<button data-test='pill' class='bg-primary text-primary-foreground'>Play</button>"
            "<button data-test='muted-pill' class='bg-foreground/10 text-foreground'>Queue</button>"
            "<div data-test='alpha' class='bg-background/55'>Alpha</div></body></html>", css];
        [probe.web loadHTMLString:html baseURL:nil];
        [NSApp run];
    }
}
