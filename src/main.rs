#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // Hide console in release

// NEcessary imports
use eframe::egui;
use rayon::prelude::*;
use std::ffi::{c_void, OsString};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, CloseHandle};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetLogicalDrives, GetDriveTypeA, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_CREATE_USN_JOURNAL};

struct SafeHandle(HANDLE);
impl Drop for SafeHandle {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

// --- RAW NTFS STRUCTURES --- for storing the values read from teh MFT table

//Similar to the USN_JOURNAL_DATA_V0 structure in C
#[repr(C)] // Tells rust compiler to use C-style memory layout
#[derive(Debug, Default)] // Can be printed with {:?} and has a default constructor
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
#[derive(Debug, Default)]
struct CreateUsnJournalData {
    maximum_size: u64,
    allocation_delta: u64,
}

// Similar to the MFT_ENUM_DATA structure in C
#[repr(C)]
struct MftEnumData {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

 // Similar to the USN_RECORD structure in C
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

const USN_RECORD_HEADER_SIZE: usize = 60;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x00000010; // A bitmask indicating a directory

// --- APP DATA STRUCTURES ---
 
// Represents a single file or directory entry in the MFT
#[derive(Clone, Debug)]
struct FileEntry {
    id: u64,
    parent_id: u64,
    name: String,
    is_dir: bool,
    drive_idx: u8,
}

// Application state enum to switch between different UI states
enum AppState {
    Initializing,
    Scanning { count: u64, current_drive: String, start_time: Instant },
    Ready,
    Error(String),
}

// Main application struct
struct DeepSearchApp {
    state: AppState,
    file_data: Arc<Vec<FileEntry>>, // Read-only after scan
    drives: Arc<Vec<String>>,
    scan_errors: Vec<String>,
    search_query: String,
    search_results: Vec<FileEntry>,
    search_stats: Option<(usize, Duration)>,
    
    // Communication
    rx_progress: crossbeam_channel::Receiver<(u64, String)>,
    tx_progress: crossbeam_channel::Sender<(u64, String)>,
    rx_data: crossbeam_channel::Receiver<(Vec<FileEntry>, Vec<String>, Vec<String>)>,
    tx_data: crossbeam_channel::Sender<(Vec<FileEntry>, Vec<String>, Vec<String>)>,
    rx_error: crossbeam_channel::Receiver<String>,
    tx_error: crossbeam_channel::Sender<String>,
    
    // Search Async
    rx_search: crossbeam_channel::Receiver<(String, Vec<FileEntry>, Duration)>,
    tx_search: crossbeam_channel::Sender<(String, Vec<FileEntry>, Duration)>,
}

// --- APP LOGIC IMPLEMENTATION ---
impl DeepSearchApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx_progress, rx_progress) = crossbeam_channel::unbounded();
        let (tx_data, rx_data) = crossbeam_channel::bounded(1);
        let (tx_error, rx_error) = crossbeam_channel::bounded(1);
        let (tx_search, rx_search) = crossbeam_channel::unbounded();

        Self {
            state: AppState::Initializing,
            file_data: Arc::new(Vec::new()),
            drives: Arc::new(Vec::new()),
            scan_errors: Vec::new(),
            search_query: String::new(),
            search_results: Vec::new(),
            search_stats: None,
            rx_progress,
            tx_progress,
            rx_data,
            tx_data,
            rx_error,
            tx_error,
            rx_search,
            tx_search,
        }
    }
    // Start scanning drives in a separate thread to prevent UI blocking 
    fn start_scan(&mut self) {
        self.state = AppState::Scanning { 
            count: 0, 
            current_drive: "Detecting drives...".to_string(),
            start_time: Instant::now() 
        };
        self.scan_errors.clear();

        let tx_progress = self.tx_progress.clone();
        let tx_data = self.tx_data.clone();
        let tx_error = self.tx_error.clone();

        thread::spawn(move || {
            match scan_all_drives(tx_progress) {
                Ok((data, drives, errors)) => {
                    let _ = tx_data.send((data, drives, errors));
                }
                Err(e) => {
                    let _ = tx_error.send(e);
                }
            }
        });
    }

    // Perform search asynchronously in a separate thread to prevent UI blocking based on current search_query
    fn perform_search(&mut self) {
        let query = self.search_query.clone();
        if query.is_empty() {
            self.search_results.clear();
            self.search_stats = None;
            return;
        }

        let data = self.file_data.clone();
        let tx = self.tx_search.clone();

        // Spawn a thread to avoid blocking the UI
        thread::spawn(move || {
            let start = Instant::now();
            let q_lower = query.to_lowercase();
            
            let results: Vec<FileEntry> = data.par_iter()
                .filter(|entry| entry.name.to_lowercase().starts_with(&q_lower))
                .cloned()
                .collect();
            
            let _ = tx.send((query, results, start.elapsed()));
        });
    }
}

// GUI Implementation

impl eframe::App for DeepSearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Set a custom theme
        let mut visuals = egui::Visuals::dark();
        visuals.window_fill = egui::Color32::from_rgb(30, 30, 35); // Dark blue-ish grey
        visuals.panel_fill = egui::Color32::from_rgb(30, 30, 35);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(30, 30, 35);
        ctx.set_visuals(visuals);

        // Handle async messages
        while let Ok((count, current_drive)) = self.rx_progress.try_recv() {
            if let AppState::Scanning { count: ref mut c, current_drive: ref mut d, .. } = self.state {
                *c = count;
                *d = current_drive;
            }
        }
        if let Ok((data, drives, errors)) = self.rx_data.try_recv() {
            self.file_data = Arc::new(data);
            self.drives = Arc::new(drives);
            self.scan_errors = errors;
            self.state = AppState::Ready;
        }
        if let Ok(err) = self.rx_error.try_recv() {
            self.state = AppState::Error(err);
        }
        
        // Handle search results
        while let Ok((query, results, duration)) = self.rx_search.try_recv() {
            // Only update if the result matches the current query (ignore old results)
            if query == self.search_query {
                self.search_stats = Some((results.len(), duration));
                self.search_results = results;
            }
            
        }

        // Auto-start scan on first frame
        if matches!(self.state, AppState::Initializing) {
            self.start_scan();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Disable text selection for labels to prevent cursor changing to I-beam
            ui.style_mut().interaction.selectable_labels = false;

            match &self.state {
                AppState::Initializing => {
                    ui.spinner();
                    ui.label("Initializing...");
                }
                AppState::Scanning { count, current_drive, start_time } => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(50.0);
                        ui.heading("Indexing MFT...");
                        ui.add_space(10.0);
                        ui.label(current_drive);
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
                        ui.add_space(5.0);
                        ui.label(egui::RichText::new("Shows hidden/system files").size(10.0).color(egui::Color32::GRAY));
                    });
                    
                    if !self.scan_errors.is_empty() {
                        ui.group(|ui| {
                            ui.set_max_width(f32::INFINITY);
                            ui.colored_label(egui::Color32::YELLOW, "⚠️ Scan Warnings:");
                            for err in &self.scan_errors {
                                ui.label(egui::RichText::new(err).size(11.0).color(egui::Color32::LIGHT_RED));
                            }
                        });
                    }

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
                        if count > 0 {
                            ui.horizontal(|ui| {
                                ui.add_space(25.0);
                                ui.label(egui::RichText::new(format!(
                                    "Found {} results in {:.3}s", 
                                    count, 
                                    duration.as_secs_f32()
                                )).size(12.0).color(egui::Color32::GRAY));
                            });
                        }
                    }

                    ui.add_space(10.0);
                    ui.separator();

                    egui::ScrollArea::vertical().show_rows(
                        ui,
                        24.0, // Fixed row height
                        self.search_results.len(),
                        |ui, row_range| {
                            // Use manual layout for full control over rows
                            ui.style_mut().spacing.item_spacing.y = 0.0;

                            for i in row_range {
                                if let Some(entry) = self.search_results.get(i) {
                                    let full_path = resolve_path(entry, &self.file_data, &self.drives);
                                    
                                    // 1. Allocate the full row area
                                    let row_height = 24.0;
                                    let (rect, response) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), row_height), 
                                        egui::Sense::click()
                                    );

                                    // 2. Handle Interaction (Click whole row to open)
                                    if response.clicked() {
                                        open_in_explorer(&full_path);
                                    }
                                    
                                    // Force pointer cursor when hovering the row
                                    let _ = response.on_hover_cursor(egui::CursorIcon::PointingHand);

                                    // 3. Paint Background (Striping + Hover)
                                    // Use rect_contains_pointer to ensure highlight works even if text captures hover
                                    let is_hovered = ui.rect_contains_pointer(rect);
                                    
                                    let bg_color = if is_hovered {
                                        Some(egui::Color32::from_rgb(40, 50, 70)) // Distinct Blue-ish hover
                                    } else if i % 2 == 1 {
                                        Some(egui::Color32::from_rgb(45, 45, 50)) // Lighter grey for striping
                                    } else {
                                        None
                                    };

                                    if let Some(color) = bg_color {
                                        ui.painter().rect_filled(rect, 0.0, color);
                                    }

                                    // 4. Draw Content
                                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                                        ui.horizontal_centered(|ui| {
                                            ui.add_space(10.0); // Padding

                                            // Icon
                                            let icon = if entry.is_dir { "📁" } else { "📄" };
                                            ui.label(icon);
                                            
                                            // Name Column (Fixed Width)
                                            let name_width = 300.0;
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(name_width, ui.available_height()),
                                                egui::Layout::left_to_right(egui::Align::Center),
                                                |ui| {
                                                    let name_text = egui::RichText::new(&entry.name).color(egui::Color32::LIGHT_BLUE);
                                                    ui.add(egui::Label::new(name_text).truncate());
                                                }
                                            );

                                            // Path Column
                                            let path_text = egui::RichText::new(&full_path).size(10.0).color(egui::Color32::GRAY);
                                            ui.add(egui::Label::new(path_text).truncate());
                                        });
                                    });
                                }
                            }
                        },
                    );
                        
                    if self.search_results.is_empty() && !self.search_query.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(20.0);
                            ui.label("No results found.");
                        });
                    }
                }
            }
        });
    }
}

fn load_icon() -> egui::IconData {
    let (icon_rgba, icon_width, icon_height) = {
        let icon = image::load_from_memory(include_bytes!("../assets/deep_search.ico"))
            .expect("Failed to open icon path")
            .into_rgba8();
        let (width, height) = icon.dimensions();
        (icon.into_raw(), width, height)
    };
    
    egui::IconData {
        rgba: icon_rgba,
        width: icon_width,
        height: icon_height,
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_icon(load_icon()),
        vsync: true, // Enable VSync to fix flickering
        ..Default::default()
    };
    eframe::run_native(
        "Deep Search",
        options,
        Box::new(|cc| Ok(Box::new(DeepSearchApp::new(cc)))),
    )
}

// --- WORKER LOGIC ---
 // Get a list of fixed drives on the system
fn get_drives() -> Vec<String> {
    let mut drives = Vec::new();
    let bitmask = unsafe { GetLogicalDrives() };
    
    for i in 0..26 {
        if (bitmask & (1 << i)) != 0 {
            let drive_letter = (b'A' + i) as char;
            let path = format!("{}:\\\0", drive_letter);
            
            let drive_type = unsafe { 
                GetDriveTypeA(PCSTR(path.as_ptr())) 
            };

            if drive_type == DRIVE_FIXED || drive_type == DRIVE_REMOVABLE {
                drives.push(format!("{}:", drive_letter));
            }
        }
    }
    drives
}

// Scan all fixed drives and return collected FileEntry data
fn scan_all_drives(
    tx_progress: crossbeam_channel::Sender<(u64, String)>
) -> Result<(Vec<FileEntry>, Vec<String>, Vec<String>), String> {
    let drives = get_drives();
    let mut all_entries = Vec::new();
    let mut errors = Vec::new();
    let mut total_count = 0;

    if drives.is_empty() {
        return Err("No fixed or removable drives found.".to_string());
    }

    for (idx, drive) in drives.iter().enumerate() {
        let _ = tx_progress.send((total_count, format!("Scanning {}...", drive)));
        
        // We ignore errors for individual drives so one bad drive doesn't stop everything
        // But if ALL fail, we might want to know.
        match scan_drive(drive, idx as u8, &tx_progress, &mut total_count) {
            Ok(entries) => all_entries.extend(entries),
            Err(e) => errors.push(format!("Failed to scan {}: {}", drive, e)),
        }
    }
    
    // Sort by (drive_idx, id) to enable binary search for parent resolution
    // This is CRITICAL for resolve_path to work correctly across multiple drives
    all_entries.par_sort_unstable_by(|a, b| {
        a.drive_idx.cmp(&b.drive_idx).then(a.id.cmp(&b.id))
    });

    Ok((all_entries, drives, errors))
}

fn scan_drive(
    drive_letter: &str, 
    drive_idx: u8,
    tx: &crossbeam_channel::Sender<(u64, String)>,
    total_count: &mut u64
) -> Result<Vec<FileEntry>, String> {
    let volume_path_str = format!("\\\\.\\{}", drive_letter);
    let volume_path: Vec<u16> = OsString::from(&volume_path_str).encode_wide().chain(Some(0)).collect();
    
    // Initial open with WRITE access to create journal if needed
    let handle_raw = unsafe {
        CreateFileW(
            PCWSTR(volume_path.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0, 
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE(ptr::null_mut()),
        )
    };

    if handle_raw == Ok(INVALID_HANDLE_VALUE) || handle_raw.is_err() {
        return Err(format!("Access Denied to {}. Run as Administrator.", drive_letter));
    }
    let mut handle = SafeHandle(handle_raw.unwrap());

    let mut journal_data = UsnJournalData::default();
    let mut bytes_returned = 0u32;
    let success = unsafe {
        DeviceIoControl(
            handle.0,
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
        // Try to create the journal if it doesn't exist
        let mut create_data = CreateUsnJournalData {
            maximum_size: 0,
            allocation_delta: 0,
        };
        let create_success = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_CREATE_USN_JOURNAL,
                Some(&mut create_data as *mut _ as *mut c_void),
                size_of::<CreateUsnJournalData>() as u32,
                None,
                0,
                Some(&mut bytes_returned),
                None,
            )
        };

        if create_success.is_err() {
             return Err(format!("Failed to query or create USN Journal on {}. Is it NTFS?", drive_letter));
        }

        // Fix 5: Drop write access - Reopen with GENERIC_READ only
        drop(handle); // Close current handle
        
        let handle_read_raw = unsafe {
            CreateFileW(
                PCWSTR(volume_path.as_ptr()),
                GENERIC_READ.0, // Read only
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                HANDLE(ptr::null_mut()),
            )
        };
        
        if handle_read_raw == Ok(INVALID_HANDLE_VALUE) || handle_read_raw.is_err() {
            return Err(format!("Failed to reopen {} for reading.", drive_letter));
        }
        handle = SafeHandle(handle_read_raw.unwrap());

        // Retry query with new handle
        let success_retry = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut journal_data as *mut _ as *mut c_void),
                size_of::<UsnJournalData>() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };
        
        if success_retry.is_err() {
             return Err(format!("Failed to query USN Journal on {} after creation attempt.", drive_letter));
        }
    }

    let mut med = MftEnumData {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: journal_data.max_usn,
    };

    let mut buffer = vec![0u8; 65536]; // 64KB Buffer
    let mut entries = Vec::with_capacity(100_000); 

    loop {
        // Fix 8: Cap total entries
        if entries.len() > 10_000_000 {
            return Err("Too many files — skipping rest for safety".to_string());
        }

        let success = unsafe {
            DeviceIoControl(
                handle.0,
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
            // Fix 1: Safe parsing
            if offset + USN_RECORD_HEADER_SIZE > bytes_returned as usize { break; }
            
            let p_record = unsafe { ptr::read_unaligned(buffer.as_ptr().add(offset) as *const UsnRecordHeader) };
            let rec_len = p_record.record_length as usize;
            
            if rec_len < USN_RECORD_HEADER_SIZE || rec_len == 0 || offset + rec_len > bytes_returned as usize { 
                // If record length is invalid, we can't trust the rest of the buffer
                break; 
            }

            let fname_len = p_record.file_name_length as usize;
            let fname_off = p_record.file_name_offset as usize;

            if fname_len > 0 {
                // Validate filename offset and length
                // Use constant 60 because size_of struct might include padding (64 bytes)
                if fname_len % 2 != 0 || fname_off < USN_RECORD_HEADER_SIZE || fname_off + fname_len > rec_len {
                    offset += rec_len;
                    continue;
                }

                let name_slice = unsafe {
                    std::slice::from_raw_parts(
                        buffer.as_ptr().add(offset + fname_off) as *const u16,
                        fname_len / 2,
                    )
                };
                
                let name = String::from_utf16_lossy(name_slice);
                
                // Fix 2: Block malicious filenames
                if name.contains(':') || name.contains("::") || name.ends_with(".lnk") || (name.contains('{') && name.contains('}')) || name.contains("\\\\?\\") {
                    offset += rec_len;
                    continue;
                }

                let is_dir = (p_record.file_attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;

                entries.push(FileEntry {
                    id: p_record.file_reference_number,
                    parent_id: p_record.parent_file_reference_number,
                    name,
                    is_dir,
                    drive_idx,
                });

                *total_count += 1;
            }
            offset += rec_len;
        }

        // Report progress every ~2k files
        if *total_count % 2_000 == 0 {
            let _ = tx.send((*total_count, format!("Scanning {}...", drive_letter)));
        }

        if bytes_returned < 8 { break; }
        med.start_file_reference_number = unsafe { ptr::read_unaligned(buffer.as_ptr() as *const u64) };
    }

    // Handle is closed automatically by SafeHandle

    Ok(entries)
}

fn resolve_path(entry: &FileEntry, data: &[FileEntry], drives: &[String]) -> String {
    let mut parts = Vec::new();
    let mut current_id = entry.id;
    let drive_idx = entry.drive_idx;
    let mut safety = 0;

    loop {
        // Binary search for (drive_idx, current_id)
        // Since data is sorted by drive_idx then id, we can find the exact entry
        let result = data.binary_search_by(|e| {
            e.drive_idx.cmp(&drive_idx).then(e.id.cmp(&current_id))
        });

        if let Ok(idx) = result {
            let e = &data[idx];

            // Stop at root (parent points to self)
            if e.parent_id == current_id {
                break;
            }

            if e.name != "." && e.name != ".." {
                parts.push(e.name.clone());
            }
            current_id = e.parent_id;
            
            safety += 1;
            if safety > 200 { break; } // Cycle/Depth protection
        } else {
            // If we can't find the parent, we assume we've reached the root.
            break;
        }
    }
    parts.reverse();
    let path = parts.join("\\");
    
    // Prepend the correct drive letter
    if let Some(drive) = drives.get(drive_idx as usize) {
        format!("{}\\{}", drive, path)
    } else {
        format!("?\\{}", path) // Fallback
    }
}


// Open the given path in Windows Explorer, selecting the file if possible
fn open_in_explorer(path: &str) {
    println!("Attempting to open: {}", path);
    
    // Fix 4: Canonicalize + validate path
    let full_path = std::fs::canonicalize(path).ok().filter(|p| {
        // Basic sanity check: must start with a drive letter
        let s = p.to_string_lossy();
        s.len() >= 3 && s.chars().nth(1) == Some(':') && s.chars().nth(2) == Some('\\')
    });

    if full_path.is_none() || !full_path.as_ref().unwrap().exists() {
        eprintln!("File does not exist or invalid path: {}", path);
        return;
    }
    let full_path = full_path.unwrap();

    let meta = std::fs::metadata(&full_path).ok();
    if meta.is_none() { return; }

    // Fix 3: Use ShellExecuteW with /select
    // This is safer than Command::spawn because it avoids cmd.exe parsing issues
    let path_str = full_path.to_string_lossy();
    let params = format!("/select,{}", path_str);
    
    let op = "open\0".encode_utf16().collect::<Vec<u16>>();
    let file = "explorer.exe\0".encode_utf16().collect::<Vec<u16>>();
    let params_wide: Vec<u16> = OsString::from(&params).encode_wide().chain(Some(0)).collect();

    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params_wide.as_ptr()),
            None,
            SW_SHOW
        );
    }
}