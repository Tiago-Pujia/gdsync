use std::sync::{Arc, Mutex};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, Status, ToolTip, Tray};

#[derive(Clone)]
pub struct GdsyncTray {
    pub status_text: Arc<Mutex<String>>,
    pub show_window_cb: Arc<dyn Fn() + Send + Sync + 'static>,
    pub open_dolphin_cb: Arc<dyn Fn() + Send + Sync + 'static>,
    pub toggle_daemon_cb: Arc<dyn Fn() + Send + Sync + 'static>,
    pub quit_cb: Arc<dyn Fn() + Send + Sync + 'static>,
}

impl Tray for GdsyncTray {
    fn id(&self) -> String {
        "gdsync".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn title(&self) -> String {
        "gdsync".into()
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_name(&self) -> String {
        "folder-gdrive".into()
    }

    fn tool_tip(&self) -> ToolTip {
        let status = self.status_text.lock().unwrap().clone();
        ToolTip {
            title: "gdsync - Google Drive FUSE".into(),
            description: status,
            icon_name: "folder-gdrive".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.show_window_cb)();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let status = self.status_text.lock().unwrap().clone();
        let show_cb = self.show_window_cb.clone();
        let dolphin_cb = self.open_dolphin_cb.clone();
        let daemon_cb = self.toggle_daemon_cb.clone();
        let quit_cb = self.quit_cb.clone();

        vec![
            MenuItem::Standard(StandardItem {
                label: status,
                enabled: false,
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "📂 Abrir en Dolphin".into(),
                activate: Box::new(move |_| {
                    dolphin_cb();
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: "📊 Abrir Panel de Control".into(),
                activate: Box::new(move |_| {
                    show_cb();
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: "⚡ Alternar Servicio (Iniciar/Detener)".into(),
                activate: Box::new(move |_| {
                    daemon_cb();
                }),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "🚪 Salir de la aplicación".into(),
                activate: Box::new(move |_| {
                    quit_cb();
                }),
                ..Default::default()
            }),
        ]
    }
}
