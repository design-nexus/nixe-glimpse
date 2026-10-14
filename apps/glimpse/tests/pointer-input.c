/* Small Wayland input helper for the Glimpse focus regression test.
 * Implements the version-1 wlr virtual-pointer wire protocol using libwayland. */
#include <wayland-client.h>
#include <string.h>
#include <stdlib.h>
static const struct wl_message pointer_requests[] = {
    {"motion", "uff", NULL}, {"motion_absolute", "uuuuu", NULL},
    {"button", "uuu", NULL}, {"axis", "uuf", NULL}, {"frame", "", NULL},
    {"axis_source", "u", NULL}, {"axis_stop", "uu", NULL},
    {"axis_discrete", "uufi", NULL}, {"destroy", "", NULL},
};
static const struct wl_interface pointer_interface = {"zwlr_virtual_pointer_v1", 1, 9, pointer_requests, 0, NULL};
static const struct wl_interface *create_types[] = {&wl_seat_interface, &pointer_interface};
static const struct wl_message manager_requests[] = {{"create_virtual_pointer", "?on", create_types}, {"destroy", "", NULL}};
static const struct wl_interface manager_interface = {"zwlr_virtual_pointer_manager_v1", 1, 2, manager_requests, 0, NULL};
static struct wl_proxy *manager;
static void global(void *data, struct wl_registry *registry, uint32_t id, const char *iface, uint32_t version) {
    if (!strcmp(iface, manager_interface.name)) manager = wl_registry_bind(registry, id, &manager_interface, 1);
}
static void removed(void *data, struct wl_registry *registry, uint32_t id) {}
static const struct wl_registry_listener listener = {global, removed};
int main(int argc, char **argv) {
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) return 1;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &listener, NULL);
    wl_display_roundtrip(display);
    if (!manager) return 2;
    struct wl_proxy *pointer = wl_proxy_marshal_flags(manager, 0, &pointer_interface, 1, 0, NULL, NULL);
    wl_proxy_marshal_flags(pointer, 0, NULL, 1, 0, 1u, wl_fixed_from_double(argc > 1 ? atof(argv[1]) : 1), wl_fixed_from_double(argc > 2 ? atof(argv[2]) : 1));
    wl_proxy_marshal_flags(pointer, 4, NULL, 1, 0);
    if (argc > 3 && !strcmp(argv[3], "click")) {
        wl_proxy_marshal_flags(pointer, 2, NULL, 1, 0, 2u, 272u, 1u);
        wl_proxy_marshal_flags(pointer, 4, NULL, 1, 0);
        wl_proxy_marshal_flags(pointer, 2, NULL, 1, 0, 3u, 272u, 0u);
        wl_proxy_marshal_flags(pointer, 4, NULL, 1, 0);
    }
    wl_display_roundtrip(display);
    wl_display_disconnect(display);
    return 0;
}
