#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::UnsafeCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::{egui, App};
use lofty::{AudioFile, Probe};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use rfd::FileDialog;

// -----------------------------------------
// CONFIG
// -----------------------------------------

const BAR_COUNT: usize = 32;
const VIS_HEIGHT: f32 = 220.0;
const RING_SIZE: usize = 1024;

// -----------------------------------------
// LOCK-FREE RING BUFFER FOR AUDIO THREAD
// -----------------------------------------

struct RingBuffer {
    data: UnsafeCell<Vec<f32>>,
    write_pos: AtomicUsize,
}

unsafe impl Sync for RingBuffer {}

impl RingBuffer {
    fn new(size: usize) -> Self {
        Self {
            data: UnsafeCell::new(vec![0.0; size]),
            write_pos: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn push(&self, value: f32) {
        let pos = self.write_pos.fetch_add(1, Ordering::Relaxed) % self.len();
        unsafe {
            (&mut *self.data.get())[pos] = value;
        }
    }

    #[inline]
    fn len(&self) -> usize {
        unsafe { (*self.data.get()).len() }
    }

    fn snapshot(&self) -> Vec<f32> {
        let len = self.len();
        let mut out = vec![0.0; len];
        let start = self.write_pos.load(Ordering::Relaxed);

        unsafe {
            let src = &*self.data.get();
            for i in 0..len {
                out[i] = src[(start + i) % len];
            }
        }

        out
    }
}

// -----------------------------------------
// VISUALIZER SOURCE
// -----------------------------------------

struct VisualizerSource<S> {
    inner: S,
    ring: Arc<RingBuffer>,
}

impl<S> Iterator for VisualizerSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if let Some(sample) = self.inner.next() {
            self.ring.push(sample);
            Some(sample)
        } else {
            None
        }
    }
}

impl<S> Source for VisualizerSource<S>
where
    S: Source<Item = f32>,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

// -----------------------------------------
// MUSIC PLAYER STRUCT
// -----------------------------------------

struct MusicPlayer {
    _stream: OutputStream,
    stream_handle: OutputStreamHandle,
    sink: Option<Arc<Sink>>,

    current_file: Option<PathBuf>,
    status_text: String,
    total_length: Option<Duration>,

    started_at: Option<Instant>,
    paused_at: Option<Instant>,
    accumulated_paused: Duration,

    volume: f32,
    seek_position: f32,

    ring: Arc<RingBuffer>,
    body: Vec<f32>,
    peak: Vec<f32>,

    // New fields for file list
    music_files: Vec<PathBuf>,
    current_directory: PathBuf,
    selected_file_index: Option<usize>,
    
    // New field to handle file selection
    file_to_select: Option<PathBuf>,
}

impl MusicPlayer {
    fn new() -> Self {
        let (_stream, stream_handle) =
            OutputStream::try_default().expect("Failed to open audio output");

        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let music_files = Self::load_music_files(&current_dir);

        Self {
            _stream,
            stream_handle,
            sink: None,
            current_file: None,
            status_text: "Select a file to start".to_string(),
            total_length: None,
            started_at: None,
            paused_at: None,
            accumulated_paused: Duration::ZERO,
            volume: 0.3,
            seek_position: 0.0,

            ring: Arc::new(RingBuffer::new(RING_SIZE)),
            body: vec![0.0; BAR_COUNT],
            peak: vec![0.0; BAR_COUNT],

            music_files,
            current_directory: current_dir,
            selected_file_index: None,
            file_to_select: None,
        }
    }

    fn load_music_files(dir: &PathBuf) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let audio_extensions = ["mp3", "wav", "flac", "ogg", "m4a", "aac", "wma"];

        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if let Some(ext_str) = ext.to_str() {
                            if audio_extensions.contains(&ext_str.to_lowercase().as_str()) {
                                files.push(path);
                            }
                        }
                    }
                }
            }
        }

        // Sort files alphabetically
        files.sort_by(|a, b| {
            a.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.file_name().unwrap_or_default().to_string_lossy().to_lowercase())
        });

        files
    }

    fn refresh_file_list(&mut self) {
        self.music_files = Self::load_music_files(&self.current_directory);
        self.selected_file_index = None;
    }

    fn open_file(&mut self) {
        if let Some(path) = FileDialog::new()
            .add_filter("Audio", &["mp3", "wav", "flac", "ogg", "m4a", "aac", "wma"])
            .pick_file()
        {
            self.set_current_file(path);
        }
    }

    fn set_current_file(&mut self, path: PathBuf) {
        self.current_file = Some(path.clone());
        self.status_text = format!(
            "Loaded: {}",
            path.file_name().unwrap().to_string_lossy()
        );
        self.read_length();
        self.seek_position = 0.0;

        self.body.fill(0.0);
        self.peak.fill(0.0);

        // Find and select the file in the list
        if let Some(index) = self.music_files.iter().position(|p| p == &path) {
            self.selected_file_index = Some(index);
        }
    }

    fn read_length(&mut self) {
        self.total_length = None;

        if let Some(path) = &self.current_file {
            if let Ok(tagged) = Probe::open(path).and_then(|p| p.read()) {
                self.total_length = Some(tagged.properties().duration());
            }
        }
    }

    fn play_from(&mut self, seconds: f32) {
        if self.current_file.is_none() {
            self.status_text = "No file selected".to_string();
            return;
        }

        let path = self.current_file.clone().unwrap();
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => {
                self.status_text = "Error: File could not be opened".to_string();
                return;
            }
        };

        let decoder = match Decoder::new(std::io::BufReader::new(file)) {
            Ok(s) => s,
            Err(_) => {
                self.status_text = "Error: Unsupported or corrupted file".to_string();
                return;
            }
        };

        let sample_rate = decoder.sample_rate();

        // New file for actual playback
        let file2 = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => {
                self.status_text = "Error: File could not be opened".to_string();
                return;
            }
        };

        let decoder2 = match Decoder::new(std::io::BufReader::new(file2)) {
            Ok(s) => s,
            Err(_) => {
                self.status_text = "Error: Unsupported or corrupted file".to_string();
                return;
            }
        };

        let mut source = decoder2.convert_samples::<f32>();

        // SEEKING
        let target_samples = (seconds * sample_rate as f32) as u64;
        for _ in 0..target_samples {
            if source.next().is_none() {
                break;
            }
        }

        // WRAP WITH VISUALIZER SOURCE
        let vis_source = VisualizerSource {
            inner: source,
            ring: self.ring.clone(),
        };

        let sink = Sink::try_new(&self.stream_handle).expect("Failed to create sink");
        sink.append(vis_source);
        sink.set_volume(self.volume);
        sink.play();

        self.sink = Some(Arc::new(sink));
        self.started_at = Some(Instant::now());
        self.paused_at = None;
        self.accumulated_paused = Duration::ZERO;
        self.seek_position = seconds;
        self.status_text = "Playing".to_string();
    }

    fn play(&mut self) {
        self.play_from(self.seek_position);
    }

    fn pause(&mut self) {
        if let Some(sink) = &self.sink {
            sink.pause();
            self.paused_at = Some(Instant::now());
            self.status_text = "Paused".to_string();
        }
    }

    fn stop(&mut self) {
        if let Some(sink) = &self.sink {
            sink.stop();
        }
        self.sink = None;

        self.started_at = None;
        self.paused_at = None;
        self.accumulated_paused = Duration::ZERO;
        self.seek_position = 0.0;

        self.body.fill(0.0);
        self.peak.fill(0.0);

        self.status_text = "Stopped".to_string();
    }

    fn set_volume(&mut self, vol: f32) {
        self.volume = vol;
        if let Some(sink) = &self.sink {
            sink.set_volume(vol);
        }
    }

    fn mute(&mut self) {
        self.set_volume(0.0);
        self.status_text = "Muted".to_string();
    }

    fn unmute(&mut self) {
        self.set_volume(0.3);
        self.status_text = "Playing".to_string();
    }

    fn current_time(&self) -> Option<Duration> {
        let start = self.started_at?;
        let now = Instant::now();

        let paused = self.paused_at.map(|p| now.duration_since(p)).unwrap_or_default();
        let elapsed = now.duration_since(start) - self.accumulated_paused - paused;

        Some(Duration::from_secs_f32(self.seek_position + elapsed.as_secs_f32()))
    }

    fn format_duration(d: Duration) -> String {
        let secs = d.as_secs();
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }

    fn update_visualizer(&mut self) {
        let samples = self.ring.snapshot();

        if samples.is_empty() {
            return;
        }

        let chunk_size = (samples.len() / BAR_COUNT).max(1);

        for i in 0..BAR_COUNT {
            let start = i * chunk_size;
            let end = std::cmp::min(start + chunk_size, samples.len());

            if start >= end {
                continue;
            }

            let slice = &samples[start..end];

            let max_val = slice
                .iter()
                .map(|&x| x.abs())
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(0.0);

            let scaled_val = max_val * 3.0;

            self.body[i] = self.body[i] * 0.7 + scaled_val * 0.3;

            if self.body[i] > self.peak[i] {
                self.peak[i] = self.body[i];
            } else {
                self.peak[i] *= 0.95;
            }

            self.body[i] = self.body[i].clamp(0.0, 1.0);
            self.peak[i] = self.peak[i].clamp(0.0, 1.0);
        }
    }

    fn draw_visualizer(&self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Visualizer")
                .size(16.0)
                .strong()
                .color(egui::Color32::from_rgb(190, 210, 255)),
        );

        let frame = egui::Frame::none()
            .fill(egui::Color32::from_rgb(18, 20, 30))
            .rounding(8.0)
            .inner_margin(egui::Margin::symmetric(8.0, 8.0))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 70, 100)));

        frame.show(ui, |ui| {
            let (rect, _response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), VIS_HEIGHT),
                egui::Sense::hover(),
            );

            let painter = ui.painter();

            // Grid background
            let grid_color = egui::Color32::from_rgb(35, 40, 60);
            let step_y = 20.0;
            let mut y = rect.bottom();
            while y > rect.top() {
                painter.line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    egui::Stroke::new(0.5, grid_color),
                );
                y -= step_y;
            }

            let bar_width = rect.width() / BAR_COUNT as f32;
            let bar_spacing = bar_width * 0.2;
            let actual_bar_width = (bar_width - bar_spacing).max(1.0);

            for i in 0..BAR_COUNT {
                let x = rect.left() + i as f32 * bar_width + bar_spacing / 2.0;

                let body_val = self.body[i];
                let peak_val = self.peak[i];

                let body_height = body_val * (rect.height() - 4.0);
                let peak_height = peak_val * (rect.height() - 4.0);

                if body_height > 0.0 {
                    let body_rect = egui::Rect::from_min_max(
                        egui::pos2(x, rect.bottom() - body_height),
                        egui::pos2(x + actual_bar_width, rect.bottom()),
                    );

                    let intensity = (body_val * 255.0) as u8;
                    let color = egui::Color32::from_rgb(
                        (intensity / 3).max(40),
                        (intensity).max(80),
                        255 - intensity / 3,
                    );

                    // Glow
                    painter.rect_filled(
                        body_rect.expand(1.5),
                        3.0,
                        egui::Color32::from_rgba_unmultiplied(
                            color.r(),
                            color.g(),
                            color.b(),
                            40,
                        ),
                    );

                    painter.rect_filled(body_rect, 3.0, color);
                }

                if peak_height > body_height && peak_height > 2.0 {
                    let peak_y = rect.bottom() - peak_height;
                    painter.line_segment(
                        [egui::pos2(x, peak_y), egui::pos2(x + actual_bar_width, peak_y)],
                        egui::Stroke::new(1.0, egui::Color32::WHITE),
                    );
                }
            }
        });
    }

    fn draw_file_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Music Library")
                .size(16.0)
                .strong()
                .color(egui::Color32::from_rgb(190, 210, 255)),
        );

        ui.add_space(4.0);

        // Current directory display
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("📁")
                    .size(14.0)
                    .color(egui::Color32::from_rgb(150, 180, 220)),
            );
            ui.label(
                egui::RichText::new(
                    self.current_directory
                        .to_string_lossy()
                        .chars()
                        .take(40)
                        .collect::<String>()
                )
                .size(12.0)
                .color(egui::Color32::from_rgb(180, 200, 230)),
            );
        });

        ui.add_space(4.0);

        // Controls for directory
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("📂 Change Dir").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(50, 120, 80)),
                )
                .clicked()
            {
                if let Some(path) = FileDialog::new().pick_folder() {
                    self.current_directory = path;
                    self.refresh_file_list();
                }
            }

            if ui
                .add(
                    egui::Button::new(egui::RichText::new("🔄 Refresh").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(60, 100, 150)),
                )
                .clicked()
            {
                self.refresh_file_list();
            }
        });

        ui.add_space(8.0);

        // File list header
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("Files: {}", self.music_files.len()))
                    .size(12.0)
                    .color(egui::Color32::from_rgb(160, 180, 210)),
            );
        });

        ui.add_space(4.0);

        // File list in a scrollable area
        let frame = egui::Frame::none()
            .fill(egui::Color32::from_rgb(18, 20, 30))
            .rounding(8.0)
            .inner_margin(egui::Margin::symmetric(4.0, 4.0))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 70, 100)));

        frame.show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(ui.available_height() - 40.0)
                .show(ui, |ui| {
                    if self.music_files.is_empty() {
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                egui::RichText::new("No music files found")
                                    .italics()
                                    .color(egui::Color32::from_rgb(130, 130, 160)),
                            );
                        });
                    } else {
                        // Store clicked file path in a temporary variable
                        let mut clicked_file: Option<PathBuf> = None;
                        
                        for (index, file_path) in self.music_files.iter().enumerate() {
                            let file_name = file_path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy();
                            
                            let is_selected = self.selected_file_index == Some(index);
                            let is_current = self.current_file.as_ref() == Some(file_path);
                            
                            let button_color = if is_selected {
                                egui::Color32::from_rgb(50, 100, 150)
                            } else if is_current {
                                egui::Color32::from_rgb(40, 90, 60)
                            } else {
                                egui::Color32::from_rgb(35, 40, 65)
                            };
                            
                            let text_color = if is_selected || is_current {
                                egui::Color32::WHITE
                            } else {
                                egui::Color32::from_rgb(200, 210, 235)
                            };
                            
                            let button = egui::Button::new(
                                egui::RichText::new(format!("🎵 {}", file_name)).color(text_color)
                            )
                                .fill(button_color)
                                .frame(false)
                                .min_size(egui::vec2(ui.available_width(), 32.0));
                            
                            if ui.add(button).clicked() {
                                clicked_file = Some(file_path.clone());
                            }
                            
                            ui.add_space(2.0);
                        }
                        
                        // Handle file selection after the loop
                        if let Some(path) = clicked_file {
                            self.file_to_select = Some(path);
                        }
                    }
                });
        });
    }
}

// -----------------------------------------
// UI + APP UPDATE
// -----------------------------------------

impl App for MusicPlayer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_visualizer();
        
        // Handle file selection if needed
        if let Some(path) = self.file_to_select.take() {
            // Find the index of the clicked file
            if let Some(index) = self.music_files.iter().position(|p| p == &path) {
                self.selected_file_index = Some(index);
                self.set_current_file(path);
                self.play();
            }
        }

        // Top bar
        egui::TopBottomPanel::top("top_bar")
            .exact_height(40.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    ui.label(
                        egui::RichText::new("🎧 Rust Music Player")
                            .size(20.0)
                            .strong()
                            .color(egui::Color32::from_rgb(210, 220, 255)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("✖").color(egui::Color32::WHITE))
                                    .fill(egui::Color32::from_rgb(180, 50, 50)),
                            )
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
            });

        // Left control panel
        egui::SidePanel::left("controls_panel")
            .resizable(false)
            .min_width(260.0)
            .max_width(320.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);

                // File section
                egui::Frame::group(ui.style())
                    .fill(egui::Color32::from_rgb(32, 34, 48))
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("File")
                                    .strong()
                                    .color(egui::Color32::from_rgb(200, 210, 240)),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.add(
                                        egui::Button::new(egui::RichText::new("📂 Open").color(egui::Color32::WHITE))
                                            .fill(egui::Color32::from_rgb(40, 160, 80))
                                    ).clicked() {
                                        self.open_file();
                                    }
                                },
                            );
                        });

                        ui.add_space(4.0);
                        if let Some(file) = &self.current_file {
                            ui.label(
                                egui::RichText::new(
                                    file.file_name().unwrap().to_string_lossy().to_string(),
                                )
                                .size(13.0)
                                .color(egui::Color32::from_rgb(190, 200, 230)),
                            );
                        } else {
                            ui.label(
                                egui::RichText::new("No file selected")
                                    .italics()
                                    .color(egui::Color32::from_rgb(130, 130, 160)),
                            );
                        }
                    });

                ui.add_space(8.0);

                // Playback section
                egui::Frame::group(ui.style())
                    .fill(egui::Color32::from_rgb(32, 34, 48))
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Playback")
                                    .strong()
                                    .color(egui::Color32::from_rgb(200, 210, 240)),
                            );
                        });

                        ui.add_space(4.0);

                        ui.horizontal(|ui| {
                            if ui.add(
                                egui::Button::new(egui::RichText::new("▶ Play").color(egui::Color32::WHITE))
                                    .fill(egui::Color32::from_rgb(40, 160, 80))
                            ).clicked() {
                                self.play();
                            }
                            if ui.add(
                                egui::Button::new(egui::RichText::new("⏸ Pause").color(egui::Color32::WHITE))
                                    .fill(egui::Color32::from_rgb(40, 160, 80))
                            ).clicked() {
                                self.pause();
                            }
                            if ui.add(
                                egui::Button::new(egui::RichText::new("⏹ Stop").color(egui::Color32::WHITE))
                                    .fill(egui::Color32::from_rgb(40, 160, 80))
                            ).clicked() {
                                self.stop();
                            }
                        });

                        ui.add_space(6.0);

                        let current = self.current_time();
                        let total = self.total_length;
                        let time_text = match (current, total) {
                            (Some(c), Some(t)) => format!(
                                "{} / {}",
                                Self::format_duration(c),
                                Self::format_duration(t)
                            ),
                            (Some(c), None) => format!("{} / --:--", Self::format_duration(c)),
                            _ => "00:00 / --:--".to_string(),
                        };

                        ui.label(
                            egui::RichText::new(time_text)
                                .size(16.0)
                                .color(egui::Color32::from_rgb(190, 220, 255)),
                        );

                        if let Some(total) = self.total_length {
                            let total_secs = total.as_secs_f32();
                            let mut pos = self.seek_position;

                            ui.add_space(4.0);
                            let slider = egui::Slider::new(&mut pos, 0.0..=total_secs)
                                .text("Position")
                                .show_value(false);
                            if ui.add(slider).changed() {
                                self.seek_position = pos;
                                self.play_from(pos);
                            }
                        }

                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&self.status_text)
                                .size(13.0)
                                .color(egui::Color32::from_rgb(160, 180, 210)),
                        );
                    });

                ui.add_space(8.0);

                // Volume section
                egui::Frame::group(ui.style())
                    .fill(egui::Color32::from_rgb(32, 34, 48))
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Volume")
                                    .strong()
                                    .color(egui::Color32::from_rgb(200, 210, 240)),
                            );
                        });

                        ui.add_space(4.0);

                        ui.horizontal(|ui| {
                            ui.label("🔊");
                            let mut vol = self.volume * 100.0;
                            if ui
                                .add(
                                    egui::Slider::new(&mut vol, 0.0..=100.0)
                                        .show_value(false),
                                )
                                .changed()
                            {
                                self.set_volume(vol / 100.0);
                            }
                            ui.label(format!("{:.0}%", vol));
                        });

                        ui.add_space(4.0);

                        ui.horizontal(|ui| {
                            if ui.add(
                                egui::Button::new(egui::RichText::new("🔇 Mute").color(egui::Color32::WHITE))
                                    .fill(egui::Color32::from_rgb(40, 160, 80))
                            ).clicked() {
                                self.mute();
                            }
                            if ui.add(
                                egui::Button::new(egui::RichText::new("🔊 Unmute").color(egui::Color32::WHITE))
                                    .fill(egui::Color32::from_rgb(40, 160, 80))
                            ).clicked() {
                                self.unmute();
                            }
                        });
                    });
            });

        // Right file list panel
        egui::SidePanel::right("file_list_panel")
            .resizable(true)
            .min_width(280.0)
            .max_width(400.0)
            .show(ctx, |ui| {
                self.draw_file_list(ui);
            });

        // Central visualizer panel
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(20, 22, 32))
                    .inner_margin(egui::Margin::symmetric(12.0, 10.0)),
            )
            .show(ctx, |ui| {
                self.draw_visualizer(ui);

                ui.add_space(10.0);
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Made by ValyR in Rust")
                            .size(14.0)
                            .italics()
                            .color(egui::Color32::from_rgb(150, 160, 200)),
                    );
                });
            });

        ctx.request_repaint();
    }
}

// -----------------------------------------
// MAIN
// -----------------------------------------

fn main() -> eframe::Result<()> {
    let app = MusicPlayer::new();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 650.0])  // Increased width for 3-column layout
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Music Player with Visualizer & Library",
        native_options,
        Box::new(|_cc| Box::new(app)),
    )
}