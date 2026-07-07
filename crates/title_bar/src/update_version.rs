use gpui::{Empty, Render};

pub struct UpdateVersion;

impl UpdateVersion {
    pub fn new(_cx: &mut gpui::Context<Self>) -> Self {
        Self
    }

    pub fn update_simulation(&mut self, _cx: &mut gpui::Context<Self>) {}

    pub fn show_update_in_menu_bar(&self) -> bool {
        false
    }
}

impl Render for UpdateVersion {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        Empty
    }
}
