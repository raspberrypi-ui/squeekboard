/*
 * Copyright (C) 2010-2011 Daiki Ueno <ueno@unixuser.org>
 * Copyright (C) 2010-2011 Red Hat, Inc.
 * Copyright (C) 2018-2019 Purism SPC
 * SPDX-License-Identifier: GPL-3.0+
 * Author: Guido Günther <agx@sigxcpu.org>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 */
#include <stdlib.h>
#include <gio/gio.h>
#include <gtk/gtk.h>
#include <glib/gi18n.h>

#include "config.h"

#include "eek/eek.h"
#include "eekboard/eekboard-context-service.h"
#include "dbus.h"
#include "layout.h"
#include "main.h"
#include "outputs.h"
#include "panel.h"
#include "submission.h"
#include "server-context-service.h"
#include "wayland.h"

#include <gdk/gdkwayland.h>


typedef enum _SqueekboardDebugFlags {
    SQUEEKBOARD_DEBUG_FLAG_NONE = 0,
    SQUEEKBOARD_DEBUG_FLAG_FORCE_SHOW    = 1 << 0,
    SQUEEKBOARD_DEBUG_FLAG_GTK_INSPECTOR = 1 << 1,
} SqueekboardDebugFlags;


/// Some state, some IO components, all mixed together.
/// Better move what's possible to state::Application,
/// or secondary data structures of the same general shape.
struct squeekboard {
    struct squeek_wayland wayland; // Just hooks.
    DBusHandler *dbus_handler; // Controls visibility of the OSK.
    EekboardContextService *settings_context; // Gsettings hooks for layouts.
    /// Gsettings hook for visibility. TODO: this does not belong in gsettings.
    ServerContextService *settings_handler;
    struct panel_manager panel_manager; // Controls the shape of the panel.
};


GMainLoop *loop;

static void
quit (void)
{
  g_main_loop_quit (loop);
}

// D-Bus

static void
on_name_acquired (GDBusConnection *connection,
                  const gchar     *name,
                  gpointer         user_data)
{
    (void)connection;
    (void)name;
    (void)user_data;
}

static void
on_name_lost (GDBusConnection *connection,
              const gchar     *name,
              gpointer         user_data)
{
    SqueekboardDebugFlags *flags = user_data;
    // TODO: could conceivable continue working
    // if intrnal changes stop sending dbus changes
    (void)connection;
    (void)name;
    (void)user_data;
    g_warning("DBus unavailable, unclear how to continue. Is Squeekboard already running?");
    if ((*flags & SQUEEKBOARD_DEBUG_FLAG_FORCE_SHOW) == 0) {
        exit (1);
    }
}

// Wayland

static void
registry_handle_global (void *data,
                        struct wl_registry *registry,
                        uint32_t name,
                        const char *interface,
                        uint32_t version)
{
    // currently only v1 supported for most interfaces,
    // so there's no reason to check for available versions.
    // Even when lower version would be served, it would not be supported,
    // causing a hard exit
    (void)version;
    struct squeek_wayland *wayland = data;

    if (!strcmp (interface, zwlr_layer_shell_v1_interface.name)) {
        wayland->layer_shell = wl_registry_bind (registry, name,
            &zwlr_layer_shell_v1_interface, 1);
    } else if (!strcmp (interface, zwp_virtual_keyboard_manager_v1_interface.name)) {
        wayland->virtual_keyboard_manager = wl_registry_bind(registry, name,
            &zwp_virtual_keyboard_manager_v1_interface, 1);
    } else if (!strcmp (interface, zwp_input_method_manager_v2_interface.name)) {
        wayland->input_method_manager = wl_registry_bind(registry, name,
            &zwp_input_method_manager_v2_interface, 1);
    } else if (!strcmp (interface, "wl_output")) {
        struct wl_output *output = wl_registry_bind (registry, name,
            &wl_output_interface, 2);
        squeek_outputs_register(wayland->outputs, output, name);
    } else if (!strcmp(interface, "wl_seat")) {
        wayland->seat = wl_registry_bind(registry, name,
            &wl_seat_interface, 1);
    }
}

static void
registry_handle_global_remove (void *data,
                               struct wl_registry *registry,
                               uint32_t name)
{
    (void)registry;
    struct squeek_wayland *wayland = data;
    struct wl_output *output = squeek_outputs_try_unregister(wayland->outputs, name);
    if (output) {
        wl_output_destroy(output);
    }
}

static const struct wl_registry_listener registry_listener = {
  registry_handle_global,
  registry_handle_global_remove
};


void init_wayland(struct squeek_wayland *wayland) {
    // Set up Wayland
    gdk_set_allowed_backends ("wayland");
    GdkDisplay *gdk_display = gdk_display_get_default ();
    struct wl_display *display = gdk_wayland_display_get_wl_display (gdk_display);

    if (display == NULL) {
        g_error ("Failed to get display: %m\n");
        exit(1);
    }

    struct wl_registry *registry = wl_display_get_registry (display);
    wl_registry_add_listener (registry, &registry_listener, wayland);
    wl_display_roundtrip(display); // wait until the registry is actually populated

    if (!wayland->seat) {
        g_error("No seat Wayland global available.");
        exit(1);
    }
    if (!wayland->virtual_keyboard_manager) {
        g_error("No virtual keyboard manager Wayland global available.");
        exit(1);
    }
    if (!wayland->layer_shell) {
        g_error("No layer shell global available.");
        exit(1);
    }

    if (!wayland->input_method_manager) {
        g_warning("Wayland input method interface not available");
    }

    if (wayland->input_method_manager) {
        wayland->input_method = zwp_input_method_manager_v2_get_input_method(
            wayland->input_method_manager,
            wayland->seat);
    }
    if (wayland->virtual_keyboard_manager) {
        wayland->virtual_keyboard = zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(
            wayland->virtual_keyboard_manager,
            wayland->seat);
    }

    // initialize global
    squeek_wayland = wayland;
}

#define SESSION_NAME "sm.puri.OSK0"

GDBusProxy *_proxy = NULL;
GDBusProxy *_client_proxy = NULL;
gchar      *_client_path = NULL;


static void
send_quit_response (GDBusProxy  *proxy)
{
    g_debug ("Calling EndSessionResponse");
    g_dbus_proxy_call (proxy, "EndSessionResponse",
        g_variant_new ("(bs)", TRUE, ""), G_DBUS_CALL_FLAGS_NONE,
        G_MAXINT, NULL, NULL, NULL);
}

static void
unregister_client (void)
{
    g_autoptr (GError) error = NULL;

    g_return_if_fail (G_IS_DBUS_PROXY (_proxy));
    g_return_if_fail (_client_path != NULL);

    g_debug ("Unregistering client");

    g_dbus_proxy_call_sync (_proxy,
			    "UnregisterClient",
			    g_variant_new ("(o)", _client_path),
			    G_DBUS_CALL_FLAGS_NONE,
			    G_MAXINT,
			    NULL,
			    &error);

    if (error) {
        g_warning ("Failed to unregister client: %s", error->message);
    }

    g_clear_object (&_client_proxy);
    g_clear_pointer (&_client_path, g_free);
}

static void client_proxy_signal (GDBusProxy  *proxy,
				 const gchar *sender_name,
				 const gchar *signal_name,
				 GVariant    *parameters,
				 gpointer     user_data)
{
    if (g_str_equal (signal_name, "QueryEndSession")) {
        g_debug ("Received QueryEndSession");
        send_quit_response (proxy);
    } else if (g_str_equal (signal_name, "CancelEndSession")) {
        g_debug ("Received CancelEndSession");
    } else if (g_str_equal (signal_name, "EndSession")) {
        g_debug ("Received EndSession");
        send_quit_response (proxy);
        unregister_client ();
	quit ();
    } else if (g_str_equal (signal_name, "Stop")) {
        g_debug ("Received Stop");
        unregister_client ();
        quit ();
    }
}

static void
session_register(void) {
    char *autostart_id = getenv("DESKTOP_AUTOSTART_ID");
    if (!autostart_id) {
        g_debug("No autostart id");
        autostart_id = "";
    }
    GError *error = NULL;
    _proxy = g_dbus_proxy_new_for_bus_sync(G_BUS_TYPE_SESSION,
        G_DBUS_PROXY_FLAGS_DO_NOT_AUTO_START, NULL,
        "org.gnome.SessionManager", "/org/gnome/SessionManager",
        "org.gnome.SessionManager", NULL, &error);
    if (error) {
        g_warning("Could not connect to session manager: %s\n",
                error->message);
        g_clear_error(&error);
        return;
    }

    g_autoptr (GVariant) res = NULL;
    res = g_dbus_proxy_call_sync(_proxy, "RegisterClient",
        g_variant_new("(ss)", SESSION_NAME, autostart_id),
        G_DBUS_CALL_FLAGS_NONE, 1000, NULL, &error);
    if (error) {
        g_warning("Could not register to session manager: %s\n",
                error->message);
        g_clear_error(&error);
        return;
    }

    g_variant_get (res, "(o)", &_client_path);
    g_debug ("Registered client at '%s'", _client_path);

    _client_proxy = g_dbus_proxy_new_for_bus_sync (G_BUS_TYPE_SESSION,
      0, NULL, "org.gnome.SessionManager", _client_path,
      "org.gnome.SessionManager.ClientPrivate", NULL, &error);
    if (error) {
        g_warning ("Failed to get client proxy: %s", error->message);
	g_clear_error (&error);
	g_free (_client_path);
	_client_path = NULL;
	return;
    }

    g_signal_connect (_client_proxy, "g-signal", G_CALLBACK (client_proxy_signal), NULL);
}


static void
phosh_theme_init (void)
{
    GtkSettings *gtk_settings;
    const char *desktop;
    gboolean phosh_session;
    g_auto (GStrv) components = NULL;

    desktop = g_getenv ("XDG_CURRENT_DESKTOP");
    if (!desktop) {
        return;
    }

    components = g_strsplit (desktop, ":", -1);
    phosh_session = g_strv_contains ((const char * const *)components, "Phosh");

    if (!phosh_session) {
        return;
    }

    gtk_settings = gtk_settings_get_default ();
    g_object_set (G_OBJECT (gtk_settings), "gtk-application-prefer-dark-theme", TRUE, NULL);
}

static GDebugKey debug_keys[] =
{
        { .key = "force-show",
          .value = SQUEEKBOARD_DEBUG_FLAG_FORCE_SHOW,
        },
        { .key = "gtk-inspector",
          .value = SQUEEKBOARD_DEBUG_FLAG_GTK_INSPECTOR,
        },
};


static SqueekboardDebugFlags
parse_debug_env (void)
{
    const char *debugenv;
    SqueekboardDebugFlags flags = SQUEEKBOARD_DEBUG_FLAG_NONE;

    debugenv = g_getenv("SQUEEKBOARD_DEBUG");
    if (!debugenv) {
        return flags;
    }

    return g_parse_debug_string(debugenv, debug_keys, G_N_ELEMENTS (debug_keys));
}


int
main (int argc, char **argv)
{
    SqueekboardDebugFlags debug_flags = SQUEEKBOARD_DEBUG_FLAG_NONE;
    g_autoptr (GError) err = NULL;
    g_autoptr(GOptionContext) opt_context = NULL;

    const GOptionEntry options [] = {
        { NULL, 0, 0, G_OPTION_ARG_NONE, NULL, NULL, NULL }
    };
    opt_context = g_option_context_new ("- A on screen keyboard");

    g_option_context_add_main_entries (opt_context, options, NULL);
    g_option_context_add_group (opt_context, gtk_get_option_group (TRUE));
    if (!g_option_context_parse (opt_context, &argc, &argv, &err)) {
        g_warning ("%s", err->message);
        return 1;
    }

    textdomain (GETTEXT_PACKAGE);
    bind_textdomain_codeset (GETTEXT_PACKAGE, "UTF-8");
    bindtextdomain (GETTEXT_PACKAGE, LOCALEDIR);

    if (!gtk_init_check (&argc, &argv)) {
        g_printerr ("Can't init GTK\n");
        exit (1);
    }

    debug_flags = parse_debug_env ();
    eek_init ();

    phosh_theme_init ();

    struct squeekboard instance = {0};

    // Also initializes wayland
    struct rsobjects rsobjects = squeek_init();

    instance.settings_context = eekboard_context_service_new(rsobjects.state_manager);

    // set up dbus

    // TODO: make dbus errors non-always-fatal
    // dbus is not strictly necessary for the useful operation
    // if text-input is used, as it can bring the keyboard in and out

    GDBusConnection *connection = NULL;
    connection = g_bus_get_sync (G_BUS_TYPE_SESSION, NULL, &err);
    if (connection == NULL) {
        g_printerr ("Can't connect to the bus: %s. "
                    "Visibility switching unavailable.", err->message);
    }
    guint owner_id = 0;
    DBusHandler *service = NULL;
    if (connection) {
        service = dbus_handler_new(connection, DBUS_SERVICE_PATH, rsobjects.state_manager);

        if (service == NULL) {
            g_printerr ("Can't create dbus server\n");
            exit (1);
        }
        instance.dbus_handler = service;

        owner_id = g_bus_own_name_on_connection (connection,
                                                 DBUS_SERVICE_INTERFACE,
                                                 G_BUS_NAME_OWNER_FLAGS_NONE,
                                                 on_name_acquired,
                                                 on_name_lost,
                                                 &debug_flags,
                                                 NULL);
        if (owner_id == 0) {
            g_printerr ("Can't own the name\n");
            exit (1);
        }
    }

    ServerContextService *setting_listener = server_context_service_new(
                rsobjects.state_manager);
    if (!setting_listener) {
        g_warning ("could not connect to gsettings");
    }

    instance.settings_handler = setting_listener;

    eekboard_context_service_set_submission(instance.settings_context, rsobjects.submission);

    instance.panel_manager = panel_manager_new(instance.settings_context,
        rsobjects.submission,
        rsobjects.state_manager,
        rsobjects.popover);

    register_ui_loop_handler(rsobjects.receiver, &instance.panel_manager, rsobjects.popover, instance.settings_context, instance.dbus_handler);

    session_register();

    if (debug_flags & SQUEEKBOARD_DEBUG_FLAG_GTK_INSPECTOR) {
        gtk_window_set_interactive_debugging (TRUE);
    }
    if (debug_flags & SQUEEKBOARD_DEBUG_FLAG_FORCE_SHOW) {
        squeek_state_send_force_visible (rsobjects.state_manager);
    }

    loop = g_main_loop_new (NULL, FALSE);
    g_main_loop_run (loop);

    if (connection) {
        if (service) {
            if (owner_id != 0) {
                g_bus_unown_name (owner_id);
            }
            g_object_unref (service);
        }
        g_object_unref (connection);
    }
    g_main_loop_unref (loop);

    return 0;
}
