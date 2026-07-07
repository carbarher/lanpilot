#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use lanpilot_agent::{AgentConfig, discover_hosts, run_agent};
use lanpilot_core::{DiscoveryResponse, Logger, StopFlag, new_stop_flag};
use lanpilot_host::{HostConfig, StreamSource, run_host};

const INTERNAL_PAIR_CODE: &str = "000000";

fn main() -> eframe::Result<()> {
    #[cfg(windows)]
    unsafe {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SetProcessDPIAware() -> i32;
        }
        let _ = SetProcessDPIAware();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 420.0])
            .with_min_inner_size([520.0, 420.0]),
        ..Default::default()
    };

    eframe::run_native(
        "LanPilot",
        options,
        Box::new(|cc| {
            let mut visuals = egui::Visuals::dark();
            visuals.window_rounding = 12.0.into();
            visuals.widgets.noninteractive.rounding = 8.0.into();
            visuals.widgets.inactive.rounding = 8.0.into();
            visuals.widgets.hovered.rounding = 8.0.into();
            visuals.widgets.active.rounding = 8.0.into();
            
            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(26, 29, 36);
            visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(38, 43, 54);
            visuals.widgets.active.bg_fill = egui::Color32::from_rgb(52, 115, 230);
            
            cc.egui_ctx.set_visuals(visuals);
            Ok(Box::<LanPilotApp>::default())
        }),
    )
}

struct LanPilotApp {
    screen: Screen,
    status_lines: Vec<String>,
    user_message: Option<String>,
    diagnostics_lines: Vec<String>,
    show_diagnostics: bool,
    favorite_host_ipv4: Option<String>,
    favorite_host_name: Option<String>,
    discovered_hosts: Vec<DiscoveryResponse>,
    selected_host_index: Option<usize>,
    active_mode: Option<Mode>,
    worker: Option<JoinHandle<Result<(), String>>>,
    stop_flag: Option<StopFlag>,
    log_rx: Option<Receiver<String>>,
    started_at: Option<Instant>,
    connection_metrics: ConnectionMetrics,
    debug_log_path: Option<PathBuf>,
    debug_log_failed: bool,
    manual_ip: String,
    pair_code: String,
    target_code: String,
    debug_log_file: Option<fs::File>,
    autostart_enabled: bool,
    auto_host_triggered: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Screen {
    #[default]
    Home,
    Hosting,
    Connecting,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Host,
    Agent,
}

#[derive(Default, Clone, Debug)]
struct ConnectionMetrics {
    discovery_ms: Option<u64>,
    candidate_count: Option<usize>,
    probe_ms: Option<u64>,
    reachable_candidates: Option<usize>,
    handshake_ms: Option<u64>,
    total_connect_ms: Option<u64>,
    retry_count: u32,
    last_retry_reason: Option<String>,
}

impl Default for LanPilotApp {
    fn default() -> Self {
        let (favorite_host_ipv4, favorite_host_name) = load_favorite_target();
        let debug_log_path = init_debug_log().ok();
        
        let mut autostart_enabled = check_autostart_task_exists();
        if autostart_enabled {
            // Actualizar silenciosamente el registro en caso de que el ejecutable haya cambiado de ruta (update)
            let _ = set_autostart_task(true);
        } else {
            // Registrar por primera vez
            if set_autostart_task(true).is_ok() {
                autostart_enabled = true;
            }
        }
        
        let auto_host_triggered = std::env::args().any(|arg| arg == "--host");
        
        Self {
            screen: Screen::Home,
            status_lines: Vec::new(),
            user_message: None,
            diagnostics_lines: Vec::new(),
            show_diagnostics: false,
            favorite_host_ipv4,
            favorite_host_name,
            discovered_hosts: Vec::new(),
            selected_host_index: None,
            active_mode: None,
            worker: None,
            stop_flag: None,
            log_rx: None,
            started_at: None,
            connection_metrics: ConnectionMetrics::default(),
            debug_log_path,
            debug_log_failed: false,
            manual_ip: String::new(),
            pair_code: INTERNAL_PAIR_CODE.to_string(),
            target_code: String::new(),
            debug_log_file: None,
            autostart_enabled,
            auto_host_triggered,
        }
    }
}

impl LanPilotApp {
    fn start_host(&mut self) {
        self.user_message = None;
        self.reset_connection_metrics();
        let (tx, rx) = mpsc::channel::<String>();
        let stop = new_stop_flag();
        let logger = Logger::new(move |line| {
            let _ = tx.send(line);
        });
        let host_name = std::env::var("COMPUTERNAME").ok();
        let config = HostConfig {
            host_name,
            pair_code: Some(self.pair_code.clone()),
            stream_source: StreamSource::Screen,
            max_stream_frames: u64::MAX,
        };
        let worker_stop = stop.clone();
        let handle = thread::spawn(move || run_host(config, logger, worker_stop));

        self.worker = Some(handle);
        self.stop_flag = Some(stop);
        self.log_rx = Some(rx);
        self.active_mode = Some(Mode::Host);
        self.started_at = Some(Instant::now());
        self.status_lines.clear();
        self.status_lines
            .push("Equipo que comparte pantalla iniciado.".to_string());
        self.status_lines
            .push("Esperando que el otro equipo pulse \"Conectarme\".".to_string());
        self.screen = Screen::Hosting;
    }

    fn search_hosts(&mut self) {
        self.user_message = None;
        self.reset_connection_metrics();
        self.status_lines.clear();
        self.status_lines
            .push("Buscando equipos disponibles en la red local...".to_string());
        match discover_hosts(&self.pair_code) {
            Ok(hosts) => {
                self.discovered_hosts = hosts;
                self.selected_host_index = self.preferred_host_index();
                if self.selected_host_index.is_none() && !self.discovered_hosts.is_empty() {
                    self.selected_host_index = Some(0);
                }
                self.status_lines.push(format!(
                    "Se encontraron {} equipo(s). Selecciona uno para conectar o usa Conexión rápida (1 clic).",
                    self.discovered_hosts.len()
                ));
            }
            Err(err) => {
                self.discovered_hosts.clear();
                self.selected_host_index = None;
                self.status_lines.push(err);
            }
        }
    }

    fn preferred_host_index(&self) -> Option<usize> {
        let favorite_ip = self.favorite_host_ipv4.as_ref()?;
        self.discovered_hosts
            .iter()
            .position(|host| &host.host_ipv4 == favorite_ip)
    }

    fn run_diagnostics(&mut self) {
        self.show_diagnostics = true;
        self.diagnostics_lines.clear();
        self.diagnostics_lines
            .push("Diagnóstico LanPilot".to_string());
        let session_name = std::env::var("SESSIONNAME").unwrap_or_else(|_| "desconocido".to_string());
        self.diagnostics_lines
            .push(format!("Sesión Windows: {session_name}"));
        if session_name.to_ascii_lowercase().starts_with("rdp-") {
            self.diagnostics_lines.push(
                "RDP detectado: evita minimizar y mantén la sesión remota desbloqueada.".to_string(),
            );
        }
        if let Some(path) = &self.debug_log_path {
            self.diagnostics_lines
                .push(format!("Log debug provisional: {}", path.display()));
        }
        match discover_hosts(&self.pair_code) {
            Ok(hosts) => {
                self.diagnostics_lines.push(format!(
                    "Hosts detectados en LAN: {}",
                    hosts.len()
                ));
                for host in hosts.into_iter().take(5) {
                    self.diagnostics_lines.push(format!(
                        "- {} ({})",
                        host.host_name, host.host_ipv4
                    ));
                }
            }
            Err(err) => {
                self.diagnostics_lines
                    .push(format!("No se detectaron hosts: {err}"));
            }
        }
        self.diagnostics_lines
            .push("Si no hay imagen: revisa permisos de captura en el equipo remoto.".to_string());
        if let Some(ms) = self.connection_metrics.discovery_ms {
            self.diagnostics_lines
                .push(format!("Tiempo de búsqueda: {ms} ms"));
        }
        if let Some(ms) = self.connection_metrics.probe_ms {
            self.diagnostics_lines
                .push(format!("Tiempo de sondeo rápido: {ms} ms"));
        }
        if let Some(ms) = self.connection_metrics.handshake_ms {
            self.diagnostics_lines
                .push(format!("Tiempo de conexión (handshake): {ms} ms"));
        }
        if let Some(ms) = self.connection_metrics.total_connect_ms {
            self.diagnostics_lines
                .push(format!("Tiempo total de conexión: {ms} ms"));
        }
        if self.connection_metrics.retry_count > 0 {
            self.diagnostics_lines.push(format!(
                "Reintentos en último intento: {}",
                self.connection_metrics.retry_count
            ));
        }
    }

    fn start_agent(&mut self) {
        self.user_message = None;
        let Some(selected_index) = self.selected_host_index else {
            self.status_lines.clear();
            self.status_lines
                .push("Primero pulsa \"Buscar equipos\" y selecciona un equipo.".to_string());
            return;
        };
        let Some(selected_host) = self.discovered_hosts.get(selected_index).cloned() else {
            self.status_lines.clear();
            self.status_lines
                .push("La selección de equipo ya no es válida. Busca de nuevo.".to_string());
            self.selected_host_index = None;
            return;
        };
        self.start_agent_for_target(selected_host.host_name.clone(), selected_host.host_ipv4.clone());
    }

    fn reconnect_favorite(&mut self) {
        self.user_message = None;
        let Some(favorite_ip) = self.favorite_host_ipv4.clone() else {
            self.status_lines.clear();
            self.status_lines
                .push("No hay equipo favorito guardado todavía.".to_string());
            return;
        };
        let favorite_name = self
            .discovered_hosts
            .iter()
            .find(|h| h.host_ipv4 == favorite_ip)
            .map(|h| h.host_name.clone())
            .or_else(|| self.favorite_host_name.clone())
            .unwrap_or_else(|| "último equipo".to_string());
        self.start_agent_for_target(favorite_name, favorite_ip);
    }

    fn start_agent_for_target(&mut self, host_name: String, host_ipv4: String) {
        self.reset_connection_metrics();
        let (tx, rx) = mpsc::channel::<String>();
        let stop = new_stop_flag();
        let logger = Logger::new(move |line| {
            let _ = tx.send(line);
        });
        let mut config = AgentConfig::with_pair_code(&self.pair_code);
        // GUI mode should keep the session open until the user presses "Detener".
        config.target_stream_frames = 0;
        config.preferred_host_ipv4 = Some(host_ipv4.clone());
        if host_name != "último equipo" {
            config.preferred_host_name = Some(host_name.clone());
        }
        let worker_stop = stop.clone();
        let handle = thread::spawn(move || run_agent(config, logger, worker_stop));

        self.worker = Some(handle);
        self.stop_flag = Some(stop);
        self.log_rx = Some(rx);
        self.active_mode = Some(Mode::Agent);
        self.started_at = Some(Instant::now());
        self.status_lines.clear();
        self.status_lines.push(format!("Conectando con {}...", host_name));
        self.status_lines.push(
            "Conexión guiada: LanPilot elegirá automáticamente la ruta más rápida.".to_string(),
        );
        self.favorite_host_ipv4 = Some(host_ipv4.clone());
        if host_name != "último equipo" {
            self.favorite_host_name = Some(host_name.clone());
        }
        if let Err(err) = save_favorite_target(
            self.favorite_host_ipv4.as_deref(),
            self.favorite_host_name.as_deref(),
        ) {
            self.status_lines
                .push(format!("No se pudo guardar equipo favorito: {err}"));
        }
        self.screen = Screen::Connecting;
    }

    fn stop_process(&mut self) {
        if let Some(stop) = &self.stop_flag {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // The worker thread is cooperative: it notices the stop flag and
        // winds itself down (closing sockets/threads) shortly after. We
        // don't block the UI thread waiting for it to finish; dropping the
        // handle here simply detaches it so it can keep exiting in the
        // background while the UI returns to the home screen immediately.
        self.worker = None;
        self.stop_flag = None;
        self.log_rx = None;
        self.active_mode = None;
        self.started_at = None;
    }

    fn drain_logs(&mut self) {
        let mut drained_lines = Vec::new();
        if let Some(rx) = &self.log_rx {
            while let Ok(line) = rx.try_recv() {
                drained_lines.push(line);
            }
        }
        for line in drained_lines {
            self.update_metrics_from_log(&line);
            self.append_debug_log_line(&line);
            self.status_lines.push(line);
            if self.status_lines.len() > 200 {
                let overflow = self.status_lines.len() - 200;
                self.status_lines.drain(0..overflow);
            }
        }
    }

    fn append_debug_log_line(&mut self, line: &str) {
        if self.debug_log_failed {
            return;
        }
        let Some(path) = self.debug_log_path.clone() else {
            return;
        };
        if self.debug_log_file.is_none() {
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => self.debug_log_file = Some(file),
                Err(err) => {
                    self.status_lines
                        .push(format!("No se pudo abrir log debug provisional: {err}"));
                    self.debug_log_failed = true;
                    return;
                }
            }
        }
        if let Some(ref mut file) = self.debug_log_file {
            let ts_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default();
            if let Err(err) = writeln!(file, "[{ts_ms}] {line}") {
                self.status_lines
                    .push(format!("No se pudo escribir en log debug provisional: {err}"));
                self.debug_log_failed = true;
            } else {
                let _ = file.flush();
            }
        }
    }

    fn reset_connection_metrics(&mut self) {
        self.connection_metrics = ConnectionMetrics::default();
    }

    fn update_metrics_from_log(&mut self, line: &str) {
        if line.starts_with("[RECONNECT]") {
            self.connection_metrics.retry_count =
                self.connection_metrics.retry_count.saturating_add(1);
            self.connection_metrics.last_retry_reason = Some(line.to_string());
        }
        if !line.starts_with("[METRIC]") {
            return;
        }
        if let Some(ms) = parse_metric_u64(line, "discovery_ms=") {
            self.connection_metrics.discovery_ms = Some(ms);
        }
        if let Some(ms) = parse_metric_u64(line, "probe_ms=") {
            self.connection_metrics.probe_ms = Some(ms);
        }
        if let Some(ms) = parse_metric_u64(line, "handshake_ms=") {
            self.connection_metrics.handshake_ms = Some(ms);
        }
        if let Some(ms) = parse_metric_u64(line, "connect_total_ms=") {
            self.connection_metrics.total_connect_ms = Some(ms);
        }
        if let Some(count) = parse_metric_u64(line, "candidates=") {
            self.connection_metrics.candidate_count = Some(count as usize);
        }
        if let Some(count) = parse_metric_u64(line, "reachable_candidates=") {
            self.connection_metrics.reachable_candidates = Some(count as usize);
        }
    }

    #[allow(dead_code)]
    fn quick_connect(&mut self) {
        self.user_message = None;
        if self.discovered_hosts.is_empty() {
            self.search_hosts();
        }
        if self.discovered_hosts.is_empty() {
            self.status_lines
                .push("No hay equipos disponibles para conexión rápida.".to_string());
            return;
        }
        let selected = self
            .preferred_host_index()
            .or(Some(0))
            .and_then(|idx| self.discovered_hosts.get(idx).cloned());
        let Some(host) = selected else {
            self.status_lines
                .push("No se pudo elegir un equipo automáticamente.".to_string());
            return;
        };
        self.selected_host_index = self
            .discovered_hosts
            .iter()
            .position(|candidate| candidate.host_ipv4 == host.host_ipv4);
        self.start_agent_for_target(host.host_name, host.host_ipv4);
    }

    fn worker_finished(&mut self) -> bool {
        let Some(handle) = &self.worker else {
            return false;
        };
        if !handle.is_finished() {
            return false;
        }
        // Safe to take + join without blocking: we just confirmed it's done.
        let handle = self.worker.take().expect("worker checked Some above");
        match handle.join() {
            Ok(Ok(())) => {
                self.status_lines.push("El proceso terminó.".to_string());
            }
            Ok(Err(err)) => {
                self.user_message = Some(humanize_error(&err));
                self.status_lines.push(format!("El proceso terminó con error: {err}"));
            }
            Err(_) => {
                self.user_message = Some("El proceso terminó de forma inesperada.".to_string());
                self.status_lines
                    .push("El proceso terminó de forma inesperada.".to_string());
            }
        }
        self.stop_flag = None;
        self.log_rx = None;
        self.active_mode = None;
        self.started_at = None;
        self.screen = Screen::Home;
        true
    }

    fn draw_home(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(16.0);
            ui.heading(egui::RichText::new("🚀 LanPilot").strong().size(26.0));
            ui.label("Controla y comparte pantallas de forma ultra-simple");
            
            let state_line = connection_state_from_logs(&self.status_lines);
            ui.colored_label(egui::Color32::from_rgb(150, 160, 180), state_line);

            if let Some(message) = &self.user_message {
                ui.add_space(8.0);
                ui.colored_label(
                    egui::Color32::from_rgb(255, 110, 110),
                    egui::RichText::new(message).strong().size(14.0),
                );
            }
            ui.add_space(20.0);

            // Layout de 2 columnas claras: Compartir (Izquierda) y Conectar (Derecha)
            ui.columns(2, |columns| {
                // Columna 1: Compartir Pantalla (Host)
                columns[0].vertical_centered(|ui| {
                    ui.group(|ui| {
                        ui.set_min_width(220.0);
                        ui.set_min_height(200.0);
                        
                        ui.add_space(12.0);
                        ui.heading("📺 Compartir");
                        ui.label("Permite que otros vean tu PC");
                        ui.add_space(16.0);

                        ui.horizontal(|ui| {
                            ui.label("Tu código:");
                            ui.add(egui::TextEdit::singleline(&mut self.pair_code).char_limit(6));
                        });
                        ui.add_space(16.0);

                        if ui
                            .add_sized([180.0, 40.0], egui::Button::new("Compartir Pantalla").fill(egui::Color32::from_rgb(52, 115, 230)))
                            .clicked()
                        {
                            self.start_host();
                        }
                        ui.add_space(12.0);
                    });
                });

                // Columna 2: Controlar a Otro (Agente)
                columns[1].vertical_centered(|ui| {
                    ui.group(|ui| {
                        ui.set_min_width(220.0);
                        ui.set_min_height(200.0);

                        ui.add_space(12.0);
                        ui.heading("🎮 Controlar");
                        ui.label("Ver y manejar otro PC");
                        ui.add_space(16.0);

                        ui.horizontal(|ui| {
                            ui.label("Código del otro:");
                            ui.add(egui::TextEdit::singleline(&mut self.target_code).char_limit(6));
                        });
                        ui.add_space(16.0);

                        if ui
                            .add_sized([180.0, 40.0], egui::Button::new("Conectar"))
                            .clicked()
                        {
                            let code = self.target_code.trim().to_string();
                            if code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()) {
                                self.pair_code = code;
                                self.status_lines.push("Buscando host por código...".to_string());
                                self.search_hosts();
                                if !self.discovered_hosts.is_empty() {
                                    self.selected_host_index = Some(0);
                                    self.start_agent();
                                } else {
                                    self.user_message = Some("No se encontró ningún equipo con ese código en la red local.".to_string());
                                }
                            } else {
                                self.user_message = Some("Por favor introduce un código de emparejamiento válido de 6 dígitos.".to_string());
                            }
                        }
                        ui.add_space(12.0);
                    });
                });
            });

            ui.add_space(20.0);

            // Sección Colapsable para Opciones Avanzadas (para no confundir a gente normal)
            ui.collapsing("⚙️ Opciones avanzadas (Técnico)", |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("IP manual:");
                    ui.text_edit_singleline(&mut self.manual_ip);
                    if ui.button("Conectar por IP").clicked() {
                        let ip = self.manual_ip.trim().to_string();
                        if !ip.is_empty() {
                            self.start_agent_for_target("IP Directa".to_string(), ip);
                        }
                    }
                });

                ui.add_space(8.0);
                #[cfg(windows)]
                {
                    let mut autostart = self.autostart_enabled;
                    if ui.checkbox(&mut autostart, "Iniciar Host automáticamente al encender PC (elevado sin UAC)").changed() {
                        match set_autostart_task(autostart) {
                            Ok(()) => {
                                self.autostart_enabled = autostart;
                                if autostart {
                                    self.status_lines.push("Inicio automático (elevado) activado con éxito.".to_string());
                                } else {
                                    self.status_lines.push("Inicio automático desactivado.".to_string());
                                }
                            }
                            Err(err) => {
                                self.user_message = Some(format!("Error de inicio automático: {}", err));
                            }
                        }
                    }
                    ui.add_space(4.0);
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Refrescar equipos").clicked() {
                        self.search_hosts();
                    }
                    if ui.button("Diagnóstico").clicked() {
                        self.run_diagnostics();
                    }
                    if self.favorite_host_ipv4.is_some() && ui.button("Reconectar último").clicked() {
                        self.reconnect_favorite();
                    }
                });

                if !self.discovered_hosts.is_empty() {
                    ui.add_space(8.0);
                    let duplicate_names = duplicate_host_names(&self.discovered_hosts);
                    let selected_text = self
                        .selected_host_index
                        .and_then(|idx| self.discovered_hosts.get(idx))
                        .map(|host| host_display_name(host, duplicate_names.contains(&host.host_name)))
                        .unwrap_or_else(|| "Selecciona un equipo".to_string());

                    ui.horizontal(|ui| {
                        egui::ComboBox::from_label("Equipos en LAN")
                            .selected_text(selected_text)
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                for (idx, host) in self.discovered_hosts.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut self.selected_host_index,
                                        Some(idx),
                                        host_display_name(host, duplicate_names.contains(&host.host_name)),
                                    );
                                }
                            });
                        if ui.button("Conectar").clicked() {
                            self.start_agent();
                        }
                    });
                }

                if self.show_diagnostics && !self.diagnostics_lines.is_empty() {
                    ui.add_space(8.0);
                    ui.group(|ui| {
                        for line in &self.diagnostics_lines {
                            ui.label(line);
                        }
                    });
                }
            });

            ui.add_space(12.0);
            if !self.status_lines.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new("Registro de estado:").strong());
                for line in self.status_lines.iter().rev().take(3).rev() {
                    ui.label(egui::RichText::new(line).size(11.0).color(egui::Color32::from_rgb(160, 170, 180)));
                }
            }
        });
    }

    fn draw_hosting(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.heading("Este equipo comparte pantalla");
            ui.add_space(8.0);
            
            // Generar URI de conexión rápida
            let local_ip = lanpilot_core::get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());
            let connection_uri = format!("lanpilot://connect?ip={}&code={}", local_ip, self.pair_code);

            ui.group(|ui| {
                ui.add_space(6.0);
                ui.label("Escanear para conectar instantáneamente:");
                ui.add_space(8.0);
                
                if let Ok(qr) = qrcodegen::QrCode::encode_text(&connection_uri, qrcodegen::QrCodeEcc::Medium) {
                    let size = qr.size();
                    let scale = 4.0; 
                    let padding = 8.0; 
                    let qr_width = size as f32 * scale;
                    let total_width = qr_width + padding * 2.0;
                    
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(total_width, total_width),
                        egui::Sense::hover(),
                    );
                    let painter = ui.painter_at(rect);
                    painter.rect_filled(rect, 8.0, egui::Color32::WHITE); 
                    
                    let qr_start = rect.min + egui::vec2(padding, padding);
                    for y in 0..size {
                        for x in 0..size {
                            if qr.get_module(x, y) {
                                let min = qr_start + egui::vec2(x as f32 * scale, y as f32 * scale);
                                let max = min + egui::vec2(scale, scale);
                                painter.rect_filled(egui::Rect::from_min_max(min, max), 0.0, egui::Color32::from_rgb(26, 29, 36)); 
                            }
                        }
                    }
                }
                ui.add_space(6.0);
            });
            ui.add_space(8.0);
            
            ui.group(|ui| {
                ui.add_space(6.0);
                ui.label(format!("Código de enlace: {}", self.pair_code));
                if let Some(started) = self.started_at {
                    ui.label(format!(
                        "Esperando conexión... {} s",
                        started.elapsed().as_secs()
                    ));
                }
                ui.add_space(6.0);
            });
            ui.add_space(8.0);
            if ui.button("Detener").clicked() {
                self.stop_process();
                self.screen = Screen::Home;
            }
            ui.add_space(12.0);
            self.draw_log_box(ui);
        });
    }

    fn draw_connecting(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.heading("Conectando");
            if let Some(started) = self.started_at {
                ui.label(format!("Tiempo: {} s", started.elapsed().as_secs()));
            }
            if let Some(ms) = self.connection_metrics.discovery_ms {
                ui.label(format!("Búsqueda: {ms} ms"));
            }
            if let Some(ms) = self.connection_metrics.probe_ms {
                ui.label(format!("Sondeo rápido: {ms} ms"));
            }
            if let Some(ms) = self.connection_metrics.handshake_ms {
                ui.label(format!("Conexión base: {ms} ms"));
            }
            if let Some(ms) = self.connection_metrics.total_connect_ms {
                ui.label(format!("Total: {ms} ms"));
            }
            if self.connection_metrics.retry_count > 0 {
                ui.label(format!(
                    "Reintentos: {}",
                    self.connection_metrics.retry_count
                ));
            }
            ui.add_space(12.0);
            if ui.button("Cancelar").clicked() {
                self.stop_process();
                self.screen = Screen::Home;
            }
            ui.add_space(16.0);
            self.draw_log_box(ui);
        });
    }

    fn draw_log_box(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.status_lines {
                    ui.label(line);
                }
            });
    }
}

impl eframe::App for LanPilotApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.auto_host_triggered {
            self.auto_host_triggered = false;
            self.start_host();
        }

        self.drain_logs();
        self.worker_finished();

        egui::CentralPanel::default().show(ctx, |ui| match self.screen {
            Screen::Home => self.draw_home(ui),
            Screen::Hosting => self.draw_hosting(ui),
            Screen::Connecting => self.draw_connecting(ui),
        });

        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl Drop for LanPilotApp {
    fn drop(&mut self) {
        self.stop_process();
    }
}

fn host_display_name(host: &DiscoveryResponse, include_ip: bool) -> String {
    if host.host_name.trim().is_empty() {
        host.host_ipv4.clone()
    } else if include_ip {
        format!("{} ({})", host.host_name, host.host_ipv4)
    } else {
        host.host_name.clone()
    }
}

fn duplicate_host_names(hosts: &[DiscoveryResponse]) -> std::collections::HashSet<String> {
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for host in hosts {
        *counts.entry(host.host_name.clone()).or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect()
}

fn connection_state_from_logs(lines: &[String]) -> &'static str {
    let Some(last) = lines.last() else {
        return "Estado: listo";
    };
    let lower = last.to_ascii_lowercase();
    if lower.contains("reconnect") || lower.contains("reintento") {
        "Estado: reintentando"
    } else if lower.contains("conectado a") || lower.contains("sesión activa") {
        "Estado: conectado"
    } else if lower.contains("no se recibieron frames")
        || lower.contains("pantallas negras")
        || lower.contains("sin imagen")
    {
        "Estado: sin imagen"
    } else if lower.contains("error") {
        "Estado: error"
    } else {
        "Estado: en espera"
    }
}

fn settings_file_path() -> Option<PathBuf> {
    let app_data = std::env::var_os("APPDATA")?;
    let mut path = PathBuf::from(app_data);
    path.push("LanPilot");
    path.push("favorite_target.txt");
    Some(path)
}

fn load_favorite_target() -> (Option<String>, Option<String>) {
    let Some(path) = settings_file_path() else {
        return (None, None);
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return (None, None);
    };
    let value = contents.trim();
    if value.is_empty() {
        return (None, None);
    }
    let mut parts = value.splitn(2, '|');
    let ip = parts.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let name = parts.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    (ip, name)
}

fn save_favorite_target(ipv4: Option<&str>, host_name: Option<&str>) -> Result<(), String> {
    let Some(path) = settings_file_path() else {
        return Err("APPDATA no está disponible".to_string());
    };
    let parent = path
        .parent()
        .ok_or_else(|| "ruta de configuración inválida".to_string())?;
    fs::create_dir_all(parent).map_err(|err| format!("crear carpeta de configuración falló: {err}"))?;
    let ip = ipv4.unwrap_or("").trim();
    let name = host_name.unwrap_or("").trim();
    fs::write(path, format!("{ip}|{name}\n")).map_err(|err| format!("guardar favorito falló: {err}"))
}

fn humanize_error(err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("pantallas negras")
        || lower.contains("no se recibieron frames")
        || lower.contains("sin imagen")
        || lower.contains("captura de pantalla no disponible")
        || lower.contains("stream timeout waiting for frames")
    {
        "Conectado, pero el equipo remoto no está enviando imagen. Si usa RDP, no minimices y deja la sesión desbloqueada.".to_string()
    } else {
        "No se pudo completar la conexión. Revisa red y permisos del equipo remoto.".to_string()
    }
}

fn parse_metric_u64(line: &str, key: &str) -> Option<u64> {
    let idx = line.find(key)?;
    let value = &line[idx + key.len()..];
    let token = value.split_whitespace().next()?;
    token.parse::<u64>().ok()
}

fn init_debug_log() -> Result<PathBuf, String> {
    let mut path = std::env::temp_dir();
    path.push("LanPilot-debug.log");
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    fs::write(&path, format!("LanPilot debug provisional iniciado: {ts_ms}\n"))
        .map_err(|err| format!("crear log debug provisional falló: {err}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_pair_code_has_expected_shape() {
        assert_eq!(INTERNAL_PAIR_CODE.len(), 6);
        assert!(INTERNAL_PAIR_CODE.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn stop_process_clears_worker_state() {
        let mut app = LanPilotApp::default();
        assert!(app.worker.is_none());
        assert!(app.stop_flag.is_none());
        app.stop_process();
        assert!(app.worker.is_none());
        assert!(app.stop_flag.is_none());
    }

    #[test]
    fn start_agent_requires_discovered_host_selection() {
        let mut app = LanPilotApp::default();
        app.start_agent();
        assert!(app.worker.is_none());
        assert_eq!(app.screen, Screen::Home);
    }

    #[test]
    fn host_display_name_prefers_machine_name() {
        let host = DiscoveryResponse::new("PC-Salon", "192.168.1.20");
        assert_eq!(host_display_name(&host, false), "PC-Salon");
        assert_eq!(host_display_name(&host, true), "PC-Salon (192.168.1.20)");
    }

    #[test]
    fn host_display_name_falls_back_to_ip_when_name_empty() {
        let host = DiscoveryResponse {
            host_name: " ".to_string(),
            ..DiscoveryResponse::new("ignored", "192.168.1.20")
        };
        assert_eq!(host_display_name(&host, false), "192.168.1.20");
    }

    #[test]
    fn humanize_error_returns_no_image_message_for_stream_errors() {
        let msg = humanize_error(
            "No se recibieron frames del equipo remoto. Revisa permisos de captura.",
        );
        assert_eq!(
            msg,
            "Conectado, pero el equipo remoto no está enviando imagen. Si usa RDP, no minimices y deja la sesión desbloqueada."
        );
    }

    #[test]
    fn duplicate_host_names_detects_conflicts() {
        let hosts = vec![
            DiscoveryResponse::new("PC-Salon", "192.168.1.20"),
            DiscoveryResponse::new("PC-Salon", "192.168.1.21"),
            DiscoveryResponse::new("PC-Oficina", "192.168.1.30"),
        ];
        let duplicates = duplicate_host_names(&hosts);
        assert!(duplicates.contains("PC-Salon"));
        assert!(!duplicates.contains("PC-Oficina"));
    }

    #[test]
    fn connection_state_detects_reconnecting() {
        let lines = vec!["[RECONNECT] attempt 1/3".to_string()];
        assert_eq!(connection_state_from_logs(&lines), "Estado: reintentando");
    }

    #[test]
    fn start_agent_spawns_worker_with_selected_host() {
        let mut app = LanPilotApp::default();
        app.discovered_hosts = vec![DiscoveryResponse::new("PC-Salon", "192.168.1.20")];
        app.selected_host_index = Some(0);
        app.start_agent();
        assert!(app.worker.is_some(), "agent should start with selected host");
        assert_eq!(app.screen, Screen::Connecting);
        app.stop_process();
    }

    #[test]
    fn reconnect_favorite_spawns_worker_when_favorite_exists() {
        let mut app = LanPilotApp::default();
        app.favorite_host_ipv4 = Some("192.168.1.22".to_string());
        app.reconnect_favorite();
        assert!(app.worker.is_some(), "favorite reconnect should start agent worker");
        assert_eq!(app.screen, Screen::Connecting);
        app.stop_process();
    }

    #[test]
    fn start_and_stop_host_worker_round_trip() {
        let mut app = LanPilotApp::default();
        app.start_host();
        assert!(app.worker.is_some());
        assert!(app.stop_flag.is_some());
        assert_eq!(app.screen, Screen::Hosting);
        // Give the worker a brief moment to bind sockets and emit banner logs.
        thread::sleep(Duration::from_millis(200));
        app.stop_process();
        assert!(app.worker.is_none());
        assert!(app.stop_flag.is_none());
    }

    #[test]
    fn worker_finished_returns_to_home_and_clears_runtime_state() {
        let mut app = LanPilotApp::default();
        app.screen = Screen::Connecting;
        app.active_mode = Some(Mode::Agent);
        app.started_at = Some(Instant::now());
        app.worker = Some(thread::spawn(|| Ok::<(), String>(())));

        let mut finished = false;
        for _ in 0..10 {
            if app.worker_finished() {
                finished = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(finished);
        assert_eq!(app.screen, Screen::Home);
        assert!(app.active_mode.is_none());
        assert!(app.started_at.is_none());
        assert!(app.log_rx.is_none());
    }

    #[test]
    fn parse_metric_u64_extracts_number_from_metric_line() {
        let line = "[METRIC] discovery_ms=321 candidates=4";
        assert_eq!(parse_metric_u64(line, "discovery_ms="), Some(321));
        assert_eq!(parse_metric_u64(line, "candidates="), Some(4));
        assert_eq!(parse_metric_u64(line, "missing="), None);
    }
}

#[cfg(windows)]
fn check_autostart_task_exists() -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("schtasks")
        .args(&["/query", "/tn", "LanPilotAutoStart"])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn check_autostart_task_exists() -> bool {
    false
}

#[cfg(windows)]
fn set_autostart_task(enable: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    if enable {
        let current_exe = std::env::current_exe()
            .map_err(|e| format!("No se pudo obtener la ruta del ejecutable: {e}"))?;
        let current_exe_str = current_exe.to_string_lossy();
        let tr_value = format!("\"{}\" --host", current_exe_str);
        
        let output = std::process::Command::new("schtasks")
            .args(&["/create", "/tn", "LanPilotAutoStart", "/tr", &tr_value, "/sc", "onlogon", "/rl", "highest", "/f"])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output()
            .map_err(|e| format!("Error al ejecutar schtasks: {e}"))?;
            
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Error de schtasks: {}", stderr.trim()));
        }
    } else {
        let output = std::process::Command::new("schtasks")
            .args(&["/delete", "/tn", "LanPilotAutoStart", "/f"])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output()
            .map_err(|e| format!("Error al ejecutar schtasks: {e}"))?;
            
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Error de schtasks: {}", stderr.trim()));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_autostart_task(_enable: bool) -> Result<(), String> {
    Err("Solo soportado en Windows".to_string())
}
