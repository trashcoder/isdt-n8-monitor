use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{self, Align, Color32, Layout, RichText, Stroke, Vec2};
use hidapi::{HidApi, HidDevice};

const VENDOR_ID: u16 = 0x28e9;
const PRODUCT_ID: u16 = 0x028a;
const REPORT_LEN: usize = 64;
const CHANNEL_COUNT: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_millis(900);
const READ_TIMEOUT_MS: i32 = 250;

fn main() -> eframe::Result<()> {
    if std::env::args().any(|arg| arg == "--install-udev-rule") {
        #[cfg(target_os = "linux")]
        if let Err(error) = install_udev_rule() {
            eprintln!("USB-Freigabe konnte nicht eingerichtet werden: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 850.0])
            .with_min_inner_size([820.0, 650.0]),
        ..Default::default()
    };

    eframe::run_native(
        "ISDT N8 Monitor",
        options,
        Box::new(|cc| Ok(Box::new(ChargerApp::new(cc)))),
    )
}

#[derive(Clone, Debug)]
struct Metrics {
    channel: usize,
    mode: u8,
    battery_present: bool,
    temperature_c: u8,
    progress: u8,
    voltage_mv: u16,
    current_ma: u16,
    resistance_mohm: u16,
    power_mw: u16,
    energy_mwh: u16,
    capacity_mah: u32,
    time_s: u32,
}

#[derive(Debug)]
enum WorkerEvent {
    Status(WorkerStatus),
    Channel(Metrics),
    Error(String),
    #[cfg(target_os = "linux")]
    NeedsPermission(String),
    PermissionResult(Result<(), String>),
}

#[derive(Clone, Copy, Debug)]
enum WorkerStatus {
    Searching,
    NotFound,
    Connected,
    #[cfg(target_os = "linux")]
    NeedsPermission,
    PermissionConfigured,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    German,
    English,
}

impl Language {
    fn text(self, german: &'static str, english: &'static str) -> &'static str {
        match self {
            Self::German => german,
            Self::English => english,
        }
    }
}

fn config_file_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData/Roaming"))
    });

    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));

    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));

    base.unwrap_or_else(std::env::temp_dir)
        .join("isdt-n8-monitor")
        .join("settings.txt")
}

fn load_language(path: &std::path::Path) -> Language {
    match std::fs::read_to_string(path).unwrap_or_default().trim() {
        "de" => Language::German,
        _ => Language::English,
    }
}

fn save_language(path: &std::path::Path, language: Language) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let value = match language {
        Language::German => "de\n",
        Language::English => "en\n",
    };
    std::fs::write(path, value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppView {
    Dashboard,
    Settings,
}

struct ChargerApp {
    channels: [Option<Metrics>; CHANNEL_COUNT],
    status: WorkerStatus,
    last_error: Option<String>,
    language: Language,
    view: AppView,
    config_path: PathBuf,
    config_error: Option<String>,
    show_permission_dialog: bool,
    permission_working: bool,
    permission_result: Option<String>,
    receiver: Receiver<WorkerEvent>,
    sender: Sender<WorkerEvent>,
}

impl ChargerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let config_path = config_file_path();
        let language = load_language(&config_path);
        let (sender, receiver) = mpsc::channel();
        let worker_sender = sender.clone();
        thread::Builder::new()
            .name("isdt-usb-reader".into())
            .spawn(move || usb_worker(worker_sender))
            .expect("USB worker thread could not be started");

        Self {
            channels: std::array::from_fn(|_| None),
            status: WorkerStatus::Searching,
            last_error: None,
            language,
            view: AppView::Dashboard,
            config_path,
            config_error: None,
            show_permission_dialog: false,
            permission_working: false,
            permission_result: None,
            receiver,
            sender,
        }
    }

    fn receive_updates(&mut self) {
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                WorkerEvent::Status(status) => {
                    self.status = status;
                    if !status_has_error(status) {
                        self.last_error = None;
                    }
                }
                WorkerEvent::Channel(metrics) => {
                    if let Some(slot) = self.channels.get_mut(metrics.channel) {
                        *slot = Some(metrics);
                    }
                }
                WorkerEvent::Error(message) => {
                    self.last_error = Some(message.clone());
                    self.status = WorkerStatus::Error;
                }
                #[cfg(target_os = "linux")]
                WorkerEvent::NeedsPermission(message) => {
                    self.last_error = Some(message.clone());
                    self.status = WorkerStatus::NeedsPermission;
                    self.permission_result = Some(message);
                    self.show_permission_dialog = true;
                }
                WorkerEvent::PermissionResult(result) => {
                    self.permission_working = false;
                    match result {
                        Ok(()) => {
                            self.show_permission_dialog = false;
                            self.permission_result = None;
                            self.last_error = None;
                            self.status = WorkerStatus::PermissionConfigured;
                        }
                        Err(message) => self.permission_result = Some(message),
                    }
                }
            }
        }
    }
}

impl eframe::App for ChargerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_updates();
        ui.ctx().request_repaint_after(Duration::from_millis(150));

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(Color32::from_rgb(12, 17, 27)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(12.0);
                        let status =
                            status_text(self.status, self.last_error.as_deref(), self.language);
                        let is_error = status_has_error(self.status);
                        let navigate = header(ui, &status, is_error, self.language, self.view);
                        if navigate {
                            self.view = if self.view == AppView::Dashboard {
                                AppView::Settings
                            } else {
                                AppView::Dashboard
                            };
                        }
                        ui.add_space(18.0);
                        match self.view {
                            AppView::Dashboard => dashboard_view(ui, &self.channels, self.language),
                            AppView::Settings => settings_view(ui, self),
                        }
                    });
            });

        if self.show_permission_dialog {
            let mut request_permission = false;
            let mut dismiss = false;
            egui::Window::new(self.language.text("USB-Zugriff einrichten", "Set up USB access"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.set_min_width(400.0);
                    ui.label(self.language.text("Linux blockiert den Zugriff der App auf das ISDT-Ladegerät.", "Linux is blocking the app from accessing the ISDT charger."));
                    ui.add_space(6.0);
                    ui.label(self.language.text("Mit der einmaligen Freigabe wird eine udev-Regel nur für das ISDT-Gerät (28e9:028a) eingerichtet. Die grafische App läuft weiterhin ohne Administratorrechte.", "One-time approval installs a udev rule for this ISDT device only (28e9:028a). The graphical app will continue running without administrator privileges."));
                    if let Some(message) = &self.permission_result {
                        ui.add_space(6.0);
                        let details = if message == "Warte auf die Systemabfrage …" || message == "Waiting for system authorization …" {
                            self.language.text("Warte auf die Systemabfrage …", "Waiting for system authorization …").to_owned()
                        } else {
                            format!("{}{}", self.language.text("Details: ", "Details: "), permission_detail(message, self.language))
                        };
                        ui.label(RichText::new(details).color(Color32::from_rgb(255, 150, 130)));
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!self.permission_working, egui::Button::new(self.language.text("Freigabe einrichten …", "Set up access …")))
                            .clicked()
                        {
                            request_permission = true;
                        }
                        if ui.button(self.language.text("Später", "Later")).clicked() {
                            dismiss = true;
                        }
                    });
                });

            if request_permission {
                self.permission_working = true;
                self.permission_result = Some(
                    self.language
                        .text(
                            "Warte auf die Systemabfrage …",
                            "Waiting for system authorization …",
                        )
                        .into(),
                );
                request_linux_permission(self.sender.clone());
            }
            if dismiss {
                self.show_permission_dialog = false;
            }
        }
    }
}

fn dashboard_view(
    ui: &mut egui::Ui,
    channels: &[Option<Metrics>; CHANNEL_COUNT],
    language: Language,
) {
    summary_row(ui, channels, language);
    ui.add_space(18.0);
    ui.heading(
        RichText::new(language.text("Batteriefächer", "Battery slots"))
            .size(21.0)
            .strong(),
    );
    ui.add_space(10.0);
    egui::ScrollArea::horizontal()
        .auto_shrink([false, false])
        .show(ui, |ui| channels_table(ui, channels, language));

    ui.add_space(12.0);
    egui::Frame::new()
        .fill(Color32::from_rgb(18, 25, 37))
        .corner_radius(10.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(language.text("USB-Eingang", "USB input"))
                        .strong()
                        .color(Color32::WHITE),
                );
                ui.label(
                    RichText::new(language.text(
                        "5,2 V / 10,0 W laut Gerätedisplay",
                        "5.2 V / 10.0 W according to charger display",
                    ))
                    .color(muted()),
                );
                ui.label(
                    RichText::new(language.text(
                        "· Noch nicht über das N8-USB-Protokoll bestätigt",
                        "· Not yet confirmed through the N8 USB protocol",
                    ))
                    .color(Color32::from_rgb(232, 183, 91)),
                );
            });
        });
    ui.add_space(8.0);
    ui.label(
        RichText::new(language.text(
            "Livewerte werden direkt vom Ladegerät gelesen. Kanalnummern im Protokoll beginnen bei 0.",
            "Live values are read directly from the charger. Protocol channel numbers start at 0.",
        ))
        .size(11.0)
        .color(muted()),
    );
}

fn settings_view(ui: &mut egui::Ui, app: &mut ChargerApp) {
    let language = app.language;
    ui.heading(
        RichText::new(language.text("Einstellungen", "Settings"))
            .size(21.0)
            .strong(),
    );
    ui.add_space(12.0);
    egui::Frame::new()
        .fill(Color32::from_rgb(20, 29, 43))
        .corner_radius(12.0)
        .inner_margin(18.0)
        .show(ui, |ui| {
            ui.set_min_width(400.0);
            ui.label(RichText::new(language.text("Sprache", "Language")).size(16.0).strong());
            ui.add_space(8.0);
            let old_language = app.language;
            ui.horizontal(|ui| {
                ui.selectable_value(&mut app.language, Language::German, "Deutsch");
                ui.selectable_value(&mut app.language, Language::English, "English");
            });

            if app.language != old_language {
                app.config_error = save_language(&app.config_path, app.language)
                    .err()
                    .map(|error| error.to_string());
            }

            ui.add_space(10.0);
            ui.label(RichText::new(language.text(
                "Die Auswahl wird automatisch gespeichert und beim nächsten Start wieder verwendet.",
                "Your selection is saved automatically and restored the next time you start the app.",
            )).color(muted()));
            if let Some(error) = &app.config_error {
                ui.add_space(8.0);
                ui.label(RichText::new(format!(
                    "{}{}",
                    app.language.text("Einstellung konnte nicht gespeichert werden: ", "Could not save setting: "),
                    error,
                )).color(Color32::from_rgb(255, 130, 130)));
            }
        });
}

fn status_text(status: WorkerStatus, error: Option<&str>, language: Language) -> String {
    match status {
        WorkerStatus::Searching => language
            .text("Suche nach ISDT N8 …", "Searching for ISDT N8 …")
            .into(),
        WorkerStatus::NotFound => language
            .text(
                "Ladegerät nicht gefunden · bitte ISDT N8 anschließen",
                "Charger not found · connect the ISDT N8",
            )
            .into(),
        WorkerStatus::Connected => language
            .text("Verbunden · lese 8 Fächer", "Connected · reading 8 slots")
            .into(),
        #[cfg(target_os = "linux")]
        WorkerStatus::NeedsPermission => language
            .text("USB-Freigabe erforderlich", "USB access required")
            .into(),
        WorkerStatus::PermissionConfigured => language
            .text(
                "USB-Freigabe eingerichtet · verbinde …",
                "USB access configured · reconnecting …",
            )
            .into(),
        WorkerStatus::Error => {
            let detail = error.unwrap_or("");
            let (prefix, message) = if let Some(message) = detail.strip_prefix("hid_init:") {
                (
                    language.text(
                        "HID konnte nicht initialisiert werden: ",
                        "Could not initialize HID: ",
                    ),
                    message,
                )
            } else if let Some(message) = detail.strip_prefix("device_open:") {
                (
                    language.text(
                        "Ladegerät konnte nicht geöffnet werden: ",
                        "Could not open charger: ",
                    ),
                    message,
                )
            } else if let Some(message) = detail.strip_prefix("query:") {
                (
                    language.text("Abfrage fehlgeschlagen: ", "Channel query failed: "),
                    message,
                )
            } else {
                (language.text("Fehler: ", "Error: "), detail)
            };
            format!("{prefix}{message}")
        }
    }
}

fn status_has_error(status: WorkerStatus) -> bool {
    match status {
        WorkerStatus::Error => true,
        #[cfg(target_os = "linux")]
        WorkerStatus::NeedsPermission => true,
        _ => false,
    }
}

fn permission_detail(message: &str, language: Language) -> String {
    if let Some(detail) = message.strip_prefix("open_device:") {
        return format!(
            "{}{}",
            language.text("Gerätezugriff: ", "Device access: "),
            detail
        );
    }
    if message == "authorization_cancelled" {
        return language
            .text(
                "Freigabe abgebrochen oder fehlgeschlagen",
                "Authorization was cancelled or failed",
            )
            .into();
    }
    if let Some(detail) = message.strip_prefix("pkexec_start:") {
        return format!(
            "{}{}",
            language.text(
                "Systemabfrage konnte nicht gestartet werden: ",
                "Could not start authorization prompt: "
            ),
            detail
        );
    }
    if let Some(detail) = message.strip_prefix("udevadm_start:") {
        return format!(
            "{}{}",
            language.text(
                "udevadm konnte nicht gestartet werden: ",
                "Could not start udevadm: "
            ),
            detail
        );
    }
    if message == "udev_reload_failed" {
        return language
            .text(
                "udev-Regeln konnten nicht neu geladen werden",
                "Could not reload udev rules",
            )
            .into();
    }
    if let Some(detail) = message.strip_prefix("udev_trigger_failed:") {
        return format!(
            "{}{}",
            language.text(
                "Geräteaktualisierung fehlgeschlagen: ",
                "Could not refresh device permissions: "
            ),
            detail
        );
    }
    message.to_owned()
}

fn header(
    ui: &mut egui::Ui,
    status: &str,
    is_error: bool,
    language: Language,
    view: AppView,
) -> bool {
    let mut navigate = false;
    egui::Frame::new()
        .fill(Color32::from_rgb(20, 29, 43))
        .corner_radius(14.0)
        .inner_margin(egui::Margin::symmetric(20, 16))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("ISDT N8")
                            .size(27.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    ui.label(
                        RichText::new(
                            language.text("Ladegerät · Live-Monitor", "Charger · Live monitor"),
                        )
                        .size(13.0)
                        .color(muted()),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let button_text = match view {
                        AppView::Dashboard => language.text("Einstellungen", "Settings"),
                        AppView::Settings => language.text("Zurück", "Back"),
                    };
                    if ui.button(button_text).clicked() {
                        navigate = true;
                    }
                    let color = if is_error {
                        Color32::from_rgb(255, 105, 105)
                    } else {
                        Color32::from_rgb(75, 215, 157)
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(status).size(12.0).color(color));
                        ui.painter().circle_filled(
                            ui.min_rect().left_center() + Vec2::new(-8.0, 0.0),
                            4.0,
                            color,
                        );
                    });
                });
            });
        });
    navigate
}

fn summary_row(ui: &mut egui::Ui, channels: &[Option<Metrics>; CHANNEL_COUNT], language: Language) {
    let charging = channels.iter().flatten().filter(|m| m.mode == 3).count();
    let occupied = channels
        .iter()
        .flatten()
        .filter(|m| m.battery_present)
        .count();
    let avg_temp = {
        let values: Vec<u8> = channels
            .iter()
            .flatten()
            .filter(|m| m.battery_present)
            .map(|m| m.temperature_c)
            .collect();
        if values.is_empty() {
            None
        } else {
            Some(values.iter().map(|v| *v as u32).sum::<u32>() / values.len() as u32)
        }
    };

    ui.columns(3, |columns| {
        summary_card(
            &mut columns[0],
            language.text("AKTIV LADEND", "CHARGING"),
            &format!("{charging} / 8"),
            Color32::from_rgb(67, 213, 155),
        );
        summary_card(
            &mut columns[1],
            language.text("BELEGTE FÄCHER", "OCCUPIED SLOTS"),
            &format!("{occupied} / 8"),
            Color32::from_rgb(101, 170, 255),
        );
        summary_card(
            &mut columns[2],
            language.text("MITTLERE TEMPERATUR", "AVERAGE TEMPERATURE"),
            &avg_temp.map_or_else(|| "—".into(), |v| format!("{v} °C")),
            Color32::from_rgb(242, 181, 91),
        );
    });
}

fn summary_card(ui: &mut egui::Ui, title: &str, value: &str, accent: Color32) {
    egui::Frame::new()
        .fill(Color32::from_rgb(20, 29, 43))
        .corner_radius(12.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_min_height(66.0);
            ui.label(RichText::new(title).size(10.0).strong().color(muted()));
            ui.add_space(4.0);
            ui.label(RichText::new(value).size(22.0).strong().color(accent));
        });
}

fn channels_table(
    ui: &mut egui::Ui,
    channels: &[Option<Metrics>; CHANNEL_COUNT],
    language: Language,
) {
    egui::Frame::new()
        .fill(Color32::from_rgb(16, 23, 35))
        .stroke(Stroke::new(1.0, Color32::from_rgb(34, 47, 66)))
        .corner_radius(10.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            egui::Grid::new("channels_table_grid")
                .striped(true)
                .spacing(Vec2::new(9.0, 10.0))
                .show(ui, |ui| {
                    for title in [
                        language.text("FACH", "SLOT"),
                        language.text("STATUS", "STATUS"),
                        language.text("FORTSCHRITT", "PROGRESS"),
                        language.text("SPANNUNG", "VOLTAGE"),
                        language.text("STROM", "CURRENT"),
                        language.text("WIDERSTAND", "RESISTANCE"),
                        language.text("TEMP.", "TEMP."),
                        language.text("KAPAZITÄT", "CAPACITY"),
                        language.text("LAUFZEIT", "TIME"),
                        language.text("LEISTUNG", "POWER"),
                        language.text("ENERGIE", "ENERGY"),
                    ] {
                        ui.label(RichText::new(title).size(10.0).strong().color(muted()));
                    }
                    ui.end_row();

                    for (index, metrics) in channels.iter().enumerate() {
                        ui.add_sized(
                            [54.0, 22.0],
                            egui::Label::new(
                                RichText::new(format!(
                                    "{} {:02}",
                                    language.text("Fach", "Slot"),
                                    index + 1
                                ))
                                .strong()
                                .color(Color32::WHITE),
                            ),
                        );

                        if let Some(m) = metrics {
                            let (status, status_color) = if m.battery_present {
                                mode_label(m.mode, language)
                            } else {
                                (language.text("LEER", "EMPTY"), muted())
                            };
                            ui.add_sized(
                                [105.0, 22.0],
                                egui::Label::new(
                                    RichText::new(status)
                                        .size(11.0)
                                        .strong()
                                        .color(status_color),
                                ),
                            );

                            let percent = if !m.battery_present {
                                0
                            } else if m.mode == 4 {
                                100
                            } else {
                                m.progress.min(100)
                            };
                            if m.battery_present {
                                ui.scope(|ui| {
                                    ui.visuals_mut().override_text_color =
                                        Some(Color32::from_rgb(12, 17, 27));
                                    ui.add_sized(
                                        [138.0, 22.0],
                                        egui::ProgressBar::new(percent as f32 / 100.0)
                                            .text(format!("{percent} %"))
                                            .fill(progress_color(m.mode))
                                            .corner_radius(6.0),
                                    );
                                });
                                table_value(
                                    ui,
                                    [72.0, 22.0],
                                    &format!("{:.3} V", m.voltage_mv as f32 / 1000.0),
                                );
                                table_value(
                                    ui,
                                    [65.0, 22.0],
                                    &format!("{:.3} A", m.current_ma as f32 / 1000.0),
                                );
                                table_value(ui, [83.0, 22.0], &format!("{} mΩ", m.resistance_mohm));
                                table_value(ui, [58.0, 22.0], &format!("{} °C", m.temperature_c));
                                table_value(ui, [78.0, 22.0], &format!("{} mAh", m.capacity_mah));
                                table_value(
                                    ui,
                                    [66.0, 22.0],
                                    &format!("{}:{:02}", m.time_s / 60, m.time_s % 60),
                                );
                                table_value(
                                    ui,
                                    [66.0, 22.0],
                                    &format!("{:.2} W", m.power_mw as f32 / 1000.0),
                                );
                                table_value(ui, [70.0, 22.0], &format!("{} mWh", m.energy_mwh));
                            } else {
                                ui.add_sized([138.0, 22.0], egui::ProgressBar::new(0.0));
                                for width in [72.0, 65.0, 83.0, 58.0, 78.0, 66.0, 66.0, 70.0] {
                                    table_value(ui, [width, 22.0], "—");
                                }
                            }
                        } else {
                            ui.add_sized(
                                [105.0, 22.0],
                                egui::Label::new(RichText::new("—").color(muted())),
                            );
                            ui.add_sized([138.0, 22.0], egui::ProgressBar::new(0.0));
                            for width in [72.0, 65.0, 83.0, 58.0, 78.0, 66.0, 66.0, 70.0] {
                                table_value(ui, [width, 22.0], "—");
                            }
                        }
                        ui.end_row();
                    }
                });
        });
}

fn table_value(ui: &mut egui::Ui, size: [f32; 2], value: &str) {
    ui.add_sized(
        size,
        egui::Label::new(
            RichText::new(value)
                .size(12.0)
                .color(Color32::from_rgb(225, 234, 246)),
        ),
    );
}

fn mode_label(mode: u8, language: Language) -> (&'static str, Color32) {
    match mode {
        0 => (
            language.text("LEER / BEREIT", "IDLE / READY"),
            Color32::from_rgb(151, 164, 183),
        ),
        1 => (
            language.text("WARTEN", "WAITING"),
            Color32::from_rgb(237, 193, 96),
        ),
        2 => (
            language.text("VERPOLT", "REVERSED"),
            Color32::from_rgb(255, 126, 126),
        ),
        3 => (
            language.text("LÄDT", "CHARGING"),
            Color32::from_rgb(75, 215, 157),
        ),
        4 => (
            language.text("GELADEN", "CHARGED"),
            Color32::from_rgb(101, 170, 255),
        ),
        5 => (
            language.text("ENTLÄDT", "DISCHARGING"),
            Color32::from_rgb(240, 151, 88),
        ),
        6 => (
            language.text("ENTLADEN", "DISCHARGED"),
            Color32::from_rgb(240, 151, 88),
        ),
        7 | 8 => (
            language.text("LAGERUNG", "STORAGE"),
            Color32::from_rgb(190, 150, 255),
        ),
        9 | 10 => (
            language.text("ZYKLUS", "CYCLE"),
            Color32::from_rgb(190, 150, 255),
        ),
        11 | 12 => (
            language.text("ANALYSE", "ANALYSIS"),
            Color32::from_rgb(190, 150, 255),
        ),
        13 => (
            language.text("AKTIVIERUNG", "ACTIVATION"),
            Color32::from_rgb(237, 193, 96),
        ),
        15 => (
            language.text("ENTSORGEN", "DESTROY"),
            Color32::from_rgb(255, 126, 126),
        ),
        17 => (
            language.text("ABGEBROCHEN", "CUT OFF"),
            Color32::from_rgb(255, 126, 126),
        ),
        _ => (
            language.text("STATUS UNBEKANNT", "UNKNOWN STATUS"),
            Color32::from_rgb(255, 126, 126),
        ),
    }
}

fn progress_color(mode: u8) -> Color32 {
    match mode {
        3 => Color32::from_rgb(67, 213, 155),
        4 => Color32::from_rgb(101, 170, 255),
        5..=8 => Color32::from_rgb(240, 151, 88),
        _ => Color32::from_rgb(113, 145, 190),
    }
}

fn muted() -> Color32 {
    Color32::from_rgb(139, 154, 175)
}

#[cfg(target_os = "linux")]
fn install_udev_rule() -> Result<(), String> {
    const RULE_PATH: &str = "/etc/udev/rules.d/70-isdt-n8-monitor.rules";
    const RULE: &str = "SUBSYSTEM==\"hidraw\", ATTRS{idVendor}==\"28e9\", ATTRS{idProduct}==\"028a\", TAG+=\"uaccess\"\n";

    std::fs::write(RULE_PATH, RULE).map_err(|error| error.to_string())?;
    let reload = std::process::Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .status()
        .map_err(|error| format!("udevadm_start:{error}"))?;
    if !reload.success() {
        return Err("udev_reload_failed".into());
    }

    let trigger = std::process::Command::new("udevadm")
        .args(["trigger", "--action=change", "--subsystem-match=hidraw"])
        .status()
        .map_err(|error| format!("udev_trigger_failed:{error}"))?;
    if !trigger.success() {
        return Err("udev_trigger_failed:please unplug and reconnect the charger".into());
    }
    Ok(())
}

fn request_linux_permission(sender: Sender<WorkerEvent>) {
    #[cfg(target_os = "linux")]
    thread::spawn(move || {
        let result = (|| {
            let executable =
                std::env::current_exe().map_err(|error| format!("pkexec_start:{error}"))?;
            let status = std::process::Command::new("pkexec")
                .arg(executable)
                .arg("--install-udev-rule")
                .status()
                .map_err(|error| format!("pkexec_start:{error}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("authorization_cancelled".into())
            }
        })();
        let _ = sender.send(WorkerEvent::PermissionResult(result));
    });

    #[cfg(not(target_os = "linux"))]
    {
        let _ = sender.send(WorkerEvent::PermissionResult(Err(
            "udev access setup is only available on Linux".into(),
        )));
    }
}

fn usb_worker(sender: Sender<WorkerEvent>) {
    #[cfg(target_os = "linux")]
    let mut permission_notified = false;
    loop {
        let _ = sender.send(WorkerEvent::Status(WorkerStatus::Searching));
        let api = match HidApi::new() {
            Ok(api) => api,
            Err(error) => {
                let _ = sender.send(WorkerEvent::Error(format!("hid_init:{error}")));
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        let device_present = api
            .device_list()
            .any(|device| device.vendor_id() == VENDOR_ID && device.product_id() == PRODUCT_ID);
        if !device_present {
            #[cfg(target_os = "linux")]
            {
                permission_notified = false;
            }
            let _ = sender.send(WorkerEvent::Status(WorkerStatus::NotFound));
            thread::sleep(Duration::from_secs(2));
            continue;
        }
        let device = match api.open(VENDOR_ID, PRODUCT_ID) {
            Ok(device) => {
                #[cfg(target_os = "linux")]
                {
                    permission_notified = false;
                }
                device
            }
            Err(error) => {
                #[cfg(target_os = "linux")]
                if !permission_notified {
                    let _ =
                        sender.send(WorkerEvent::NeedsPermission(format!("open_device:{error}")));
                    permission_notified = true;
                }
                #[cfg(not(target_os = "linux"))]
                let _ = sender.send(WorkerEvent::Error(format!("device_open:{error}")));
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };

        let _ = sender.send(WorkerEvent::Status(WorkerStatus::Connected));
        loop {
            let mut failed = false;
            for channel in 0..CHANNEL_COUNT {
                match query_channel(&device, channel) {
                    Ok(metrics) => {
                        if sender.send(WorkerEvent::Channel(metrics)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender
                            .send(WorkerEvent::Error(format!("query:{}:{error}", channel + 1)));
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
        drop(device);
        thread::sleep(Duration::from_secs(2));
    }
}

fn query_channel(device: &HidDevice, channel: usize) -> Result<Metrics, String> {
    let channel = channel as u8;
    let checksum = (0x12_u16 + 0x02 + 0xDE + channel as u16) as u8;
    let mut request = [0_u8; REPORT_LEN];
    request[..8].copy_from_slice(&[0x01, 0x06, 0xAA, 0x12, 0x02, 0xDE, channel, checksum]);
    device.write(&request).map_err(|e| e.to_string())?;

    let mut report = [0_u8; REPORT_LEN];
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(900) {
        let count = device
            .read_timeout(&mut report, READ_TIMEOUT_MS)
            .map_err(|e| e.to_string())?;
        if count == 0 {
            continue;
        }
        if let Some(packet) = extract_metrics(&report[..count], channel) {
            return decode_metrics(packet, channel);
        }
    }
    Err("keine passende Antwort erhalten".into())
}

fn extract_metrics(report: &[u8], channel: u8) -> Option<&[u8]> {
    let start = report.windows(2).position(|w| w == [0xAA, 0x21])?;
    if report.get(start).copied()? != 0xAA || report.get(start + 1).copied()? != 0x21 {
        return None;
    }
    let packet_len = *report.get(start + 2)? as usize;
    let packet_start = start + 3;
    let packet_end = packet_start.checked_add(packet_len)?;
    let packet = report.get(packet_start..packet_end)?;
    if packet.len() < 26 || packet[0] != 0xDF || packet[1] != channel {
        return None;
    }
    Some(&packet[..26])
}

fn decode_metrics(packet: &[u8], channel: u8) -> Result<Metrics, String> {
    if packet.len() != 26 {
        return Err(format!("unerwartete Paketlänge {}", packet.len()));
    }
    Ok(Metrics {
        channel: channel as usize,
        mode: packet[2],
        battery_present: packet[2] != 0 || packet[4] != 4,
        temperature_c: packet[5],
        progress: packet[7],
        voltage_mv: u16::from_le_bytes([packet[8], packet[9]]),
        current_ma: u16::from_le_bytes([packet[10], packet[11]]),
        resistance_mohm: u16::from_le_bytes([packet[12], packet[13]]),
        power_mw: u16::from_le_bytes([packet[14], packet[15]]),
        energy_mwh: u16::from_le_bytes([packet[16], packet[17]]),
        capacity_mah: u32::from_le_bytes([packet[18], packet[19], packet[20], packet[21]]),
        time_s: u32::from_le_bytes([packet[22], packet[23], packet[24], packet[25]]),
    })
}
