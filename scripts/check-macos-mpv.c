// Playback probe for a temporary app copy; load only the supplied bundled libmpv.
// Compile with mpv development headers, but do NOT link against -lmpv.
#include <mpv/client.h>
#include <dlfcn.h>
#include <stdio.h>
#include <time.h>

#define API(name) __typeof__(&name) api_##name = dlsym(library, #name); \
    if (!api_##name) { fprintf(stderr, "Missing %s\n", #name); return 1; }

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: check-macos-mpv bundled-libmpv audio-file-or-url [...]\n");
        return 2;
    }
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) { fprintf(stderr, "%s\n", dlerror()); return 1; }
    API(mpv_create); API(mpv_set_option_string); API(mpv_initialize);
    API(mpv_command); API(mpv_wait_event); API(mpv_set_property);
    API(mpv_get_property); API(mpv_terminate_destroy); API(mpv_error_string);
    mpv_handle *mpv = api_mpv_create();
    if (!mpv) return 1;
    // Use the actual macOS audio output, muted, without reading user mpv configuration.
    const char *options[][2] = {{"config","no"},{"vo","null"},{"vid","no"},
                               {"ao","coreaudio"},{"volume","0"},{"ytdl","no"}};
    for (unsigned i=0; i<sizeof(options)/sizeof(options[0]); i++) {
        if (api_mpv_set_option_string(mpv, options[i][0], options[i][1]) < 0) return 1;
    }
    int rc = api_mpv_initialize(mpv);
    if (rc < 0) { fprintf(stderr, "initialize: %s\n", api_mpv_error_string(rc)); return 1; }
    int failed = 0;
    for (int i=2; i<argc; i++) {
        const char *load[] = {"loadfile", argv[i], "replace", NULL};
        rc = api_mpv_command(mpv, load);
        if (rc < 0) { fprintf(stderr, "loadfile: %s\n", api_mpv_error_string(rc)); failed=1; break; }
        time_t deadline = time(NULL) + 25;
        int loaded=0, progressed=0, ended=0;
        while (time(NULL) < deadline) {
            mpv_event *event = api_mpv_wait_event(mpv, 0.1);
            if (event->event_id == MPV_EVENT_FILE_LOADED) {
                loaded=1;
                int paused=1, observed=0;
                if (api_mpv_set_property(mpv,"pause",MPV_FORMAT_FLAG,&paused)<0 ||
                    api_mpv_get_property(mpv,"pause",MPV_FORMAT_FLAG,&observed)<0 || !observed) {
                    fprintf(stderr,"pause verification failed\n"); failed=1; break;
                }
                paused=0;
                if (api_mpv_set_property(mpv,"pause",MPV_FORMAT_FLAG,&paused)<0 ||
                    api_mpv_get_property(mpv,"pause",MPV_FORMAT_FLAG,&observed)<0 || observed) {
                    fprintf(stderr,"resume verification failed\n"); failed=1; break;
                }
                const char *seek[] = {"seek","0.5","absolute",NULL};
                if (api_mpv_command(mpv,seek)<0) { failed=1; break; }
            }
            double position=0;
            if (loaded && api_mpv_get_property(mpv,"time-pos",MPV_FORMAT_DOUBLE,&position)>=0 && position>0.6)
                progressed=1;
            if (event->event_id == MPV_EVENT_END_FILE) {
                mpv_event_end_file *end=event->data;
                if (end->reason != MPV_END_FILE_REASON_EOF || end->error<0) {
                    fprintf(stderr,"end-file: reason=%d error=%s\n",end->reason,api_mpv_error_string(end->error));
                    failed=1;
                }
                ended=1; break;
            }
        }
        if (failed || !loaded || !progressed || !ended) {
            fprintf(stderr,"FAIL input %d: loaded=%d progressed=%d ended=%d\n",i-1,loaded,progressed,ended);
            failed=1; break;
        }
        printf("PASS input %d: loaded, paused, resumed, sought, decoded to EOF via muted CoreAudio\n",i-1);
        fflush(stdout);
    }
    api_mpv_terminate_destroy(mpv);
    return failed;
}
