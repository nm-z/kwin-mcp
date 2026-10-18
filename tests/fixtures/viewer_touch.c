#include <wayland-client.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <linux/input-event-codes.h>
#include "fake-input-client.h"

static struct org_kde_kwin_fake_input *input;
static void global(void *data, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version) {
    (void)data;
    if (strcmp(interface, "org_kde_kwin_fake_input") == 0)
        input = wl_registry_bind(registry, name, &org_kde_kwin_fake_input_interface, version < 6 ? version : 6);
}
static void removed(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener listener = {global, removed};
int main(int argc, char **argv) {
    const int resize = argc == 7 && strcmp(argv[1], "--resize") == 0;
    const int hold = argc == 5 && strcmp(argv[1], "--hold") == 0;
    if (argc != 4 && !resize && !hold) return 2;
    struct wl_display *display = wl_display_connect(argv[resize || hold ? 2 : 1]);
    if (!display) { perror("wl_display_connect"); return 1; }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &listener, NULL);
    if (wl_display_roundtrip(display) < 0 || !input) return 1;
    org_kde_kwin_fake_input_authenticate(input, "kwin-mcp viewer regression", "isolated viewer touch mapping");
    wl_display_roundtrip(display);
    if (hold) {
        const long seconds = strtol(argv[3], NULL, 10);
        if (seconds < 1 || seconds > 120) return 2;
        FILE *ready = fopen(argv[4], "w");
        if (!ready) { perror("touch-ready"); return 1; }
        fclose(ready);
        for (long step = 0; step < seconds * 10; ++step) {
            if (wl_display_roundtrip(display) < 0) break;
            usleep(100000);
        }
        wl_display_disconnect(display);
        return 0;
    }
    if (resize) {
        const double x = strtod(argv[3], NULL), y = strtod(argv[4], NULL);
        const double to_x = strtod(argv[5], NULL), to_y = strtod(argv[6], NULL);
        org_kde_kwin_fake_input_pointer_motion_absolute(input, wl_fixed_from_double(x), wl_fixed_from_double(y));
        org_kde_kwin_fake_input_keyboard_key(input, KEY_LEFTMETA, 1);
        org_kde_kwin_fake_input_button(input, BTN_RIGHT, 1);
        wl_display_roundtrip(display);
        for (int step = 1; step <= 12; ++step) {
            org_kde_kwin_fake_input_pointer_motion_absolute(input,
                wl_fixed_from_double(x + (to_x - x) * step / 12),
                wl_fixed_from_double(y + (to_y - y) * step / 12));
            wl_display_roundtrip(display);
            usleep(30000);
        }
        org_kde_kwin_fake_input_button(input, BTN_RIGHT, 0);
        org_kde_kwin_fake_input_keyboard_key(input, KEY_LEFTMETA, 0);
        wl_display_roundtrip(display);
        wl_display_disconnect(display);
        return 0;
    }
    org_kde_kwin_fake_input_touch_down(input, 1, wl_fixed_from_double(strtod(argv[2], NULL)), wl_fixed_from_double(strtod(argv[3], NULL)));
    org_kde_kwin_fake_input_touch_frame(input);
    wl_display_roundtrip(display);
    usleep(100000);
    org_kde_kwin_fake_input_touch_up(input, 1);
    org_kde_kwin_fake_input_touch_frame(input);
    wl_display_roundtrip(display);
    usleep(100000);
    wl_display_disconnect(display);
    return 0;
}
