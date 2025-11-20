#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // Hide console in release

use eframe::egui;
use rayon::prelude::*;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use windows::core::PCSTR;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileA, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL};
use windows::Win32::UI::Shell::ShellExecuteA;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

// --- RAW NTFS STRUCTURES ---
#[repr(C)]
#[derive(Debug, Default)]
struct UsnJournalData {
    usn_journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

#[repr(C)]
struct MftEnumData {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UsnRecordHeader {
    record_length: u32,
    major_version: u16,
    minor_version: u16,
    file_reference_number: u64,
    parent_file_reference_number: u64,
    usn: i64,
    timestamp: i64,
    reason: u32,
    source_info: u32,
    security_id: u32,
    file_attributes: u32,
    file_name_length: u16,
    file_name_offset: u16,
}

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x00000010;

// --- APP DATA STRUCTURES ---

#[derive(Clone, Debug)]
struct FileEntry {
    id: u64,
    parent_id: u64,
    name: String,
    is_dir: bool,
}

struct SearchResult {
    entry: FileEntry,
    full_path: String,
}

enum AppState {
    Initializing,
    Scanning { count: u64, start_time: Instant },
    Ready,
    Error(String),
}

struct DeepSearchApp {
    state: AppState,
    file_data: Arc<Vec<FileEntry>>, // Read-only after scan
    search_query: String,
    search_results: Vec<SearchResult>,
    search_stats: Option<(usize, Duration)>,
    
    // Communication
    rx_progress: crossbeam_channel::Receiver<u64>,
    tx_progress: crossbeam_channel::Sender<u64>,
    rx_data: crossbeam_channel::Receiver<Vec<FileEntry>>,
    tx_data: crossbeam_channel::Sender<Vec<FileEntry>>,
    rx_error: crossbeam_channel::Receiver<String>,
    tx_error: crossbeam_channel::Sender<String>,
}

impl DeepSearchApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx_progress, rx_progress) = crossbeam_channel::unbounded();
        let (tx_data, rx_data) = crossbeam_channel::bounded(1);
        let (tx_error, rx_error) = crossbeam_channel::bounded(1);

        Self {
            state: AppState::Initializing,
            file_data: Arc::new(Vec::new()),
            search_query: String::new(),
            search_results: Vec::new(),
            search_stats: None,
            rx_progress,
            tx_progress,
            rx_data,
            tx_data,
            rx_error,
            tx_error,
        }
    }

    fn start_scan(&mut self) {
        self.state = AppState::Scanning { 
            count: 0, 
            start_time: Instant::now() 
        };

        let tx_progress = self.tx_progress.clone();
        let tx_data = self.tx_data.clone();
        let tx_error = self.tx_error.clone();

        thread::spawn(move || {
            match scan_mft_worker(tx_progress) {
                Ok(data) => {
                    let _ = tx_data.send(data);
                }
                Err(e) => {
                    let _ = tx_error.send(e);
                }
            }
        });
    }

    fn perform_search(&mut self) {
        if self.search_query.is_empty() {
            self.search_results.clear();
            self.search_stats = None;
            return;
        }

        let start = Instant::now();
        let query = self.search_query.to_lowercase();
        let data = &self.file_data;

        // Parallel search for speed
        let results: Vec<FileEntry> = data.par_iter()
            .filter(|entry| entry.name.to_lowercase().contains(&query))
            .collect::<Vec<_>>() // Collect all matches first
            .into_iter()
            .take(100) // Limit results for UI performance
            .cloned()
            .collect();

        // Resolve paths for the results
        self.search_results = results.into_iter().map(|entry| {
            let full_path = resolve_path(entry.id, data);
            SearchResult {
                entry,
                full_path,
            }
        }).collect();

        self.search_stats = Some((self.search_results.len(), start.elapsed()));
    }
}

impl eframe::App for DeepSearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Set a custom theme
        let mut visuals = egui::Visuals::dark();
        visuals.window_fill = egui::Color32::from_rgb(30, 30, 35); // Dark blue-ish grey
        visuals.panel_fill = egui::Color32::from_rgb(30, 30, 35);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(30, 30, 35);
        ctx.set_visuals(visuals);

        // Handle async messages
        if let Ok(count) = self.rx_progress.try_recv() {
            if let AppState::Scanning { count: ref mut c, .. } = self.state {
                *c = count;
            }
        }
        if let Ok(data) = self.rx_data.try_recv() {
            self.file_data = Arc::new(data);
            self.state = AppState::Ready;
        }
        if let Ok(err) = self.rx_error.try_recv() {
            self.state = AppState::Error(err);
        }

        // Auto-start scan on first frame
        if matches!(self.state, AppState::Initializing) {
            self.start_scan();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            match &self.state {
                AppState::Initializing => {
                    ui.spinner();
                    ui.label("Initializing...");
                }
                AppState::Scanning { count, start_time } => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(50.0);
                        ui.heading("Indexing MFT...");
                        ui.add_space(20.0);
                        ui.spinner();
                        ui.add_space(20.0);
                        ui.label(format!("Files found: {}", count));
                        ui.label(format!("Time elapsed: {:.1}s", start_time.elapsed().as_secs_f32()));
                    });
                    ctx.request_repaint(); // Animate spinner
                }
                AppState::Error(msg) => {
                    ui.colored_label(egui::Color32::RED, format!("Error: {}", msg));
                    if ui.button("Retry").clicked() {
                        self.start_scan();
                    }
                }
                AppState::Ready => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        ui.heading("Deep Search");
                    });
                    ui.add_space(10.0);
                    
                    // Search Bar
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.search_query)
                                .hint_text("Type to search...")
                                .desired_width(f32::INFINITY)
                                .min_size(egui::vec2(0.0, 30.0)) // Taller
                        );
                        if response.changed() {
                            self.perform_search();
                        }
                        ui.add_space(20.0);
                    });

                    // Stats
                    if let Some((count, duration)) = self.search_stats {
                        ui.horizontal(|ui| {
                            ui.add_space(25.0);
                            ui.label(egui::RichText::new(format!(
                                "Found {} results in {:.3}s", 
                                count, 
                                duration.as_secs_f32()
                            )).size(12.0).color(egui::Color32::GRAY));
                        });
                    }

                    ui.add_space(10.0);
                    ui.separator();

                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add_space(10.0);
                        egui::Grid::new("results_grid")
                            .num_columns(2)
                            .spacing([10.0, 10.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for res in &self.search_results {
                                    // Icon & Name
                                    ui.horizontal(|ui| {
                                        ui.add_space(10.0);
                                        let icon = if res.entry.is_dir { "📁" } else { "📄" };
                                        ui.label(icon);
                                        if ui.link(&res.entry.name).clicked() {
                                            open_in_explorer(&res.full_path);
                                        }
                                    });

                                    // Path
                                    ui.label(egui::RichText::new(&res.full_path).size(10.0).color(egui::Color32::GRAY));
                                    ui.end_row();
                                }
                            });
                        
                        if self.search_results.is_empty() && !self.search_query.is_empty() {
                            ui.vertical_centered(|ui| {
                                ui.add_space(20.0);
                                ui.label("No results found.");
                            });
                        }
                    });
                }
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([800.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Deep Search",
        options,
        Box::new(|cc| Ok(Box::new(DeepSearchApp::new(cc)))),
    )
}

// --- WORKER LOGIC ---

fn scan_mft_worker(tx: crossbeam_channel::Sender<u64>) -> Result<Vec<FileEntry>, String> {
    let volume_path = "\\\\.\\C:\0";
    
    let handle = unsafe {
        CreateFileA(
            PCSTR(volume_path.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0, 
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE(ptr::null_mut()),
        )
    };

    if handle == Ok(INVALID_HANDLE_VALUE) || handle.is_err() {
        return Err("Access Denied. Run as Administrator.".to_string());
    }
    let handle = handle.unwrap();

    let mut journal_data = UsnJournalData::default();
    let mut bytes_returned = 0u32;
    let success = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut journal_data as *mut _ as *mut c_void),
            size_of::<UsnJournalData>() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };

    if success.is_err() {
        return Err("Failed to query USN Journal.".to_string());
    }

    let mut med = MftEnumData {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: journal_data.max_usn,
    };

    let mut buffer = vec![0u8; 65536]; 
    let mut entries = Vec::with_capacity(500_000); // Pre-allocate some space
    let mut total_count = 0;

    loop {
        let success = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_ENUM_USN_DATA,
                Some(&mut med as *mut _ as *mut c_void),
                size_of::<MftEnumData>() as u32,
                Some(buffer.as_mut_ptr() as *mut c_void),
                buffer.len() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        if success.is_err() { break; }

        let mut offset = 8; 
        while offset < bytes_returned as usize {
            let p_record = unsafe { &*(buffer.as_ptr().add(offset) as *const UsnRecordHeader) };
            let fname_len = p_record.file_name_length as usize;
            
            if fname_len > 0 {
                let name_slice = unsafe {
                    std::slice::from_raw_parts(
                        buffer.as_ptr().add(offset + p_record.file_name_offset as usize) as *const u16,
                        fname_len / 2,
                    )
                };
                
                let name = String::from_utf16_lossy(name_slice);
                let is_dir = (p_record.file_attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;

                entries.push(FileEntry {
                    id: p_record.file_reference_number,
                    parent_id: p_record.parent_file_reference_number,
                    name,
                    is_dir,
                });

                total_count += 1;
            }
            offset += p_record.record_length as usize;
        }

        // Report progress every ~10k files
        if total_count % 10_000 == 0 {
            let _ = tx.send(total_count);
        }

        med.start_file_reference_number = unsafe { ptr::read(buffer.as_ptr() as *const u64) };
    }

    unsafe { windows::Win32::Foundation::CloseHandle(handle) };

    // Sort by ID to enable binary search for parent resolution
    entries.par_sort_unstable_by_key(|e| e.id);

    Ok(entries)
}

fn resolve_path(mut current_id: u64, data: &[FileEntry]) -> String {
    let mut parts = Vec::new();
    let mut safety = 0;

    loop {
        // Binary search for the current ID
        if let Ok(idx) = data.binary_search_by_key(&current_id, |e| e.id) {
            let entry = &data[idx];
            parts.push(entry.name.clone());

            if entry.parent_id == current_id || safety > 50 {
                break; // Root or cycle
            }
            current_id = entry.parent_id;
            safety += 1;
        } else {
            parts.push("?".to_string());
            break;
        }
    }
    parts.reverse();
    let path = parts.join("\\");
    if !path.starts_with("C:") {
        format!("C:{}", path)
    } else {
        path
    }
}

fn open_in_explorer(path: &str) {
    // Use "explorer /select,path" to highlight the file
    let cmd = format!("/select,\"{}\"\0", path);
    let operation = "open\0";
    let file = "explorer.exe\0";
    
    unsafe {
        ShellExecuteA(
            windows::Win32::Foundation::HWND(ptr::null_mut()),
            PCSTR(operation.as_ptr()),
            PCSTR(file.as_ptr()),
            PCSTR(cmd.as_ptr()),
            PCSTR(ptr::null()),
            SW_SHOWNORMAL,
        );
    }
}