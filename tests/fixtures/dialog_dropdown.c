/* A modal dialog whose dropdown opens over it, for screenshot (#983).
 *
 * dialog_dropdown plain REPORT: the dropdown is a GTK_WINDOW_POPUP with no
 * type hint, transient for the dialog, the way wxWidgets makes
 * wxPopupTransientWindow (Bambu Studio's filament mapping). Under X11 it is
 * an override-redirect window that KWin neither lists nor links to the dialog.
 *
 * dialog_dropdown combo: the dropdown is the menu of a combo box, under
 * Wayland an xdg_popup of the dialog.
 *
 * dialog_dropdown cover REPORT X Y WIDTH HEIGHT: another program's
 * override-redirect window over that rectangle of the screen (X11).
 *
 * The main window is blue, the dialog green, every dropdown magenta and the
 * cover cyan. A click on the button or combo box at the dialog's centre opens
 * its dropdown. REPORT receives "x y width height", the screen rectangle of
 * the mapped dropdown or cover. */
#include <gtk/gtk.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum {
	PARENT_WIDTH = 900,
	PARENT_HEIGHT = 640,
	DIALOG_WIDTH = 520,
	DIALOG_HEIGHT = 420,
	OPENER_WIDTH = 240,
	OPENER_HEIGHT = 96,
	/* Taller than the space below the button: the dropdown hangs past the
	 * dialog's bottom edge, as long lists do. */
	DROPDOWN_WIDTH = 240,
	DROPDOWN_HEIGHT = 220,
};

static const char STYLE[] =
	"window.parent { background-image: none; background-color: #1a3a8a; }\n"
	"window.dialog, window.dialog button, window.dialog combobox button {\n"
	"\tbackground-image: none; background-color: #2e7d32; color: #ffffff; }\n"
	"window.dropdown, menu, .menu, menuitem, menuitem:hover {\n"
	"\tbackground-image: none; background-color: #ff00ff; color: #000000; }\n"
	"window.cover { background-image: none; background-color: #00ffff; }\n";

static const char *const FILAMENTS[] = {
	"PLA Basic", "PLA Matte", "PETG HF", "ABS", "TPU 95A", "PLA-CF", NULL,
};

static const char *report_path;
static GtkWidget *dropdown;

static void add_class(GtkWidget *widget, const char *name) {
	gtk_style_context_add_class(gtk_widget_get_style_context(widget), name);
}

/* Write the mapped window's screen rectangle, whole or not at all. */
static gboolean report_mapped(GtkWidget *widget, GdkEvent *event, gpointer data) {
	(void)event;
	(void)data;
	GdkWindow *window = gtk_widget_get_window(widget);
	int x = 0;
	int y = 0;
	gdk_window_get_origin(window, &x, &y);
	char partial[4096];
	snprintf(partial, sizeof partial, "%s.partial", report_path);
	FILE *file = fopen(partial, "w");
	if (!file) {
		perror(partial);
		return FALSE;
	}
	fprintf(file, "%d %d %d %d\n", x, y, gdk_window_get_width(window), gdk_window_get_height(window));
	if (fclose(file) != 0 || rename(partial, report_path) != 0) {
		perror(report_path);
	}
	return FALSE;
}

static GtkWidget *popup_window(const char *style_class) {
	GtkWidget *window = gtk_window_new(GTK_WINDOW_POPUP);
	gtk_window_set_resizable(GTK_WINDOW(window), FALSE);
	add_class(window, style_class);
	g_signal_connect(window, "map-event", G_CALLBACK(report_mapped), NULL);
	return window;
}

/* Like wxPopupTransientWindow: a popup window transient for the dialog,
 * placed below the button in root coordinates, with no type hint. */
static void open_plain(GtkWidget *opener, gpointer data) {
	GtkWidget *dialog = data;
	if (dropdown) {
		return;
	}
	int x = 0;
	int y = 0;
	int origin_x = 0;
	int origin_y = 0;
	gtk_widget_translate_coordinates(opener, dialog, 0, 0, &x, &y);
	gdk_window_get_origin(gtk_widget_get_window(dialog), &origin_x, &origin_y);
	dropdown = popup_window("dropdown");
	gtk_window_set_transient_for(GTK_WINDOW(dropdown), GTK_WINDOW(dialog));
	gtk_window_group_add_window(gtk_window_get_group(GTK_WINDOW(dialog)), GTK_WINDOW(dropdown));
	gtk_widget_set_size_request(dropdown, DROPDOWN_WIDTH, DROPDOWN_HEIGHT);
	int below = origin_y + y + gtk_widget_get_allocated_height(opener);
	gtk_window_move(GTK_WINDOW(dropdown), origin_x + x, below);
	gtk_widget_show_all(dropdown);
}

static gboolean show_dialog(gpointer data) {
	GtkWidget *dialog = data;
	gtk_widget_show_all(dialog);
	return G_SOURCE_REMOVE;
}

static GtkWidget *dialog_window(GtkWidget *parent, gboolean combo) {
	GtkWidget *dialog = gtk_window_new(GTK_WINDOW_TOPLEVEL);
	gtk_window_set_title(GTK_WINDOW(dialog), "Dropdown dialog");
	gtk_window_set_type_hint(GTK_WINDOW(dialog), GDK_WINDOW_TYPE_HINT_DIALOG);
	gtk_window_set_transient_for(GTK_WINDOW(dialog), GTK_WINDOW(parent));
	gtk_window_set_modal(GTK_WINDOW(dialog), TRUE);
	gtk_window_set_resizable(GTK_WINDOW(dialog), FALSE);
	gtk_widget_set_size_request(dialog, DIALOG_WIDTH, DIALOG_HEIGHT);
	add_class(dialog, "dialog");
	GtkWidget *opener;
	if (combo) {
		opener = gtk_combo_box_text_new();
		for (const char *const *name = FILAMENTS; *name; ++name) {
			gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(opener), *name);
		}
		gtk_combo_box_set_active(GTK_COMBO_BOX(opener), 0);
	} else {
		opener = gtk_button_new_with_label("Open dropdown");
		g_signal_connect(opener, "clicked", G_CALLBACK(open_plain), dialog);
	}
	gtk_widget_set_size_request(opener, OPENER_WIDTH, OPENER_HEIGHT);
	gtk_widget_set_halign(opener, GTK_ALIGN_CENTER);
	gtk_widget_set_valign(opener, GTK_ALIGN_CENTER);
	gtk_container_add(GTK_CONTAINER(dialog), opener);
	return dialog;
}

static int usage(void) {
	fputs("usage: dialog_dropdown plain REPORT | combo | cover REPORT X Y WIDTH HEIGHT\n", stderr);
	return 2;
}

int main(int argc, char **argv) {
	gtk_init(&argc, &argv);
	GtkCssProvider *provider = gtk_css_provider_new();
	GError *error = NULL;
	if (!gtk_css_provider_load_from_data(provider, STYLE, -1, &error)) {
		fprintf(stderr, "style: %s\n", error->message);
		return 1;
	}
	GtkStyleProvider *style = GTK_STYLE_PROVIDER(provider);
	gtk_style_context_add_provider_for_screen(gdk_screen_get_default(), style, GTK_STYLE_PROVIDER_PRIORITY_USER);
	if (argc == 7 && strcmp(argv[1], "cover") == 0) {
		report_path = argv[2];
		GtkWidget *cover = popup_window("cover");
		gtk_widget_set_size_request(cover, atoi(argv[5]), atoi(argv[6]));
		gtk_window_move(GTK_WINDOW(cover), atoi(argv[3]), atoi(argv[4]));
		gtk_widget_show_all(cover);
		gtk_main();
		return 0;
	}
	gboolean combo = argc == 2 && strcmp(argv[1], "combo") == 0;
	if (!combo && !(argc == 3 && strcmp(argv[1], "plain") == 0)) {
		return usage();
	}
	report_path = combo ? NULL : argv[2];
	GtkWidget *parent = gtk_window_new(GTK_WINDOW_TOPLEVEL);
	gtk_window_set_title(GTK_WINDOW(parent), "Dropdown parent");
	gtk_window_set_default_size(GTK_WINDOW(parent), PARENT_WIDTH, PARENT_HEIGHT);
	add_class(parent, "parent");
	g_signal_connect(parent, "destroy", G_CALLBACK(gtk_main_quit), NULL);
	gtk_widget_show_all(parent);
	/* Map the dialog after its parent, as an application opens one. */
	g_timeout_add(500, show_dialog, dialog_window(parent, combo));
	gtk_main();
	return 0;
}
