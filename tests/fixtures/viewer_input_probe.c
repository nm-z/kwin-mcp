#include <gtk/gtk.h>
#include <stdio.h>

static const char *output_path;

static void draw(GtkDrawingArea *area, cairo_t *cr, int width, int height, gpointer data) {
    (void)area;
    (void)data;
    const double colors[4][3] = {{1, 0, 0}, {0, 1, 0}, {0, 0, 1}, {1, 1, 0}};
    for (int i = 0; i < 4; ++i) {
        cairo_set_source_rgb(cr, colors[i][0], colors[i][1], colors[i][2]);
        cairo_rectangle(cr, (i % 2) * width / 2.0, (i / 2) * height / 2.0,
                        width / 2.0, height / 2.0);
        cairo_fill(cr);
    }
}

static void pressed(GtkGestureClick *gesture, int count, double x, double y, gpointer data) {
    (void)count;
    GtkWidget *area = data;
    FILE *output = fopen(output_path, "a");
    if (output) {
        fprintf(output, "%.3f %.3f %d %d %u\n", x, y, gtk_widget_get_width(area),
                gtk_widget_get_height(area), gtk_gesture_single_get_current_button(GTK_GESTURE_SINGLE(gesture)));
        fclose(output);
    }
}

static void activate(GtkApplication *app, gpointer data) {
    (void)data;
    GtkWidget *window = gtk_application_window_new(app);
    gtk_window_set_title(GTK_WINDOW(window), "Viewer input probe");
    gtk_window_set_decorated(GTK_WINDOW(window), FALSE);
    GtkWidget *area = gtk_drawing_area_new();
    gtk_drawing_area_set_draw_func(GTK_DRAWING_AREA(area), draw, NULL, NULL);
    GtkGesture *click = gtk_gesture_click_new();
    gtk_gesture_single_set_button(GTK_GESTURE_SINGLE(click), 0);
    g_signal_connect(click, "pressed", G_CALLBACK(pressed), area);
    gtk_widget_add_controller(area, GTK_EVENT_CONTROLLER(click));
    gtk_window_set_child(GTK_WINDOW(window), area);
    gtk_window_fullscreen(GTK_WINDOW(window));
    gtk_window_present(GTK_WINDOW(window));
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    output_path = argv[1];
    GtkApplication *app = gtk_application_new("org.kwinmcp.ViewerInputProbe", G_APPLICATION_NON_UNIQUE);
    g_signal_connect(app, "activate", G_CALLBACK(activate), NULL);
    int result = g_application_run(G_APPLICATION(app), 1, argv);
    g_object_unref(app);
    return result;
}
