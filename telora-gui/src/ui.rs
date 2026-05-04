use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, CssProvider, Label};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

#[derive(Clone)]
pub struct Osd {
    window: ApplicationWindow,
    label: Label,
    provider: CssProvider,
}

impl Osd {
    pub fn new(app: &Application) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .default_width(150)
            .default_height(40)
            .build();

        // Inicializar Layer Shell ANTES de realizar la ventana
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);

        // Anclar a la esquina inferior derecha
        window.set_anchor(Edge::Bottom, true);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Top, false);
        window.set_anchor(Edge::Left, false);

        // Márgenes desde los bordes
        window.set_margin(Edge::Bottom, 20);
        window.set_margin(Edge::Right, 20);

        let label = Label::new(Some("Telora Ready"));
        label.set_margin_top(10);
        label.set_margin_bottom(10);
        label.set_margin_start(20);
        label.set_margin_end(20);

        // Initial CSS
        let provider = CssProvider::new();
        provider.load_from_data("window { background-color: black; color: white; font-weight: bold; padding: 10px; border-radius: 8px; font-size: 14px; } label { color: white; }");

        let context = window.style_context();
        context.add_provider(&provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

        window.set_child(Some(&label));

        Self {
            window,
            label,
            provider,
        }
    }

    pub fn show(&self, text: &str, color: &str) {
        self.label.set_text(text);

        let css = format!(
            "window {{ background-color: {}; color: white; font-weight: bold; border-radius: 8px; font-size: 14px; }}",
            color
        );
        self.provider.load_from_data(&css);

        self.window.present();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }
}
