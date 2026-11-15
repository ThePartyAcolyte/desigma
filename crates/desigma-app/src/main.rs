// Release builds are a windowed app: double-clicking opens no command prompt. Debug builds stay
// console programs. Tests keep the console too, so `cargo test --release` can still print.
#![cfg_attr(all(windows, not(debug_assertions), not(test)), windows_subsystem = "windows")]

mod app;
mod board;
mod bridge;
mod canvas;
mod cli;
mod clipboard;
mod collab;
mod comments;
mod comp_ops;
mod crash;
mod decks;
mod demo;
mod dev;
mod edit;
mod extwatch;
mod file_dialog;
mod find;
mod fonts;
mod history;
mod home;
mod i18n;
mod icons;
mod instance;
mod io;
mod layout;
mod libraries;
#[cfg(test)]
mod lint;
mod loopback;
#[cfg(target_os = "macos")]
mod macos;
mod mcp_link;
mod motion;
mod ops;
mod panels;
mod plugins;
mod present;
mod proto;
mod protovars;
mod richtext;
mod shell;
mod showcase;
mod styles;
mod textedit;
mod textmeasure;
mod theme;
mod unsaved;
mod updater;
mod util;
mod variables;
mod vecfixture;
mod vecops;
mod video;
mod widgets;

use std::sync::atomic::Ordering;

/// The running version: the release tag that CI stamps in as `DESIGMA_VERSION`, else the crate version.
pub const VERSION: &str = match option_env!("DESIGMA_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
/// Only release builds carry a stamped version; builds from source never look for updates.
pub const RELEASE_BUILD: bool = option_env!("DESIGMA_VERSION").is_some();

/// The app icon (256 px PNG embedded at build time) for the taskbar, Dock and alt-tab. Every window
/// desigma opens (including Present and "open in new window" processes) starts through `main`, so
/// they all get it.
fn window_icon() -> Option<egui::IconData> {
    // macOS shows the window icon in the Dock when desigma runs as a bare binary (cargo run), where
    // the squircle needs the same transparent margin the .icns has; elsewhere the edge-to-edge art
    // fills title bars and taskbars as intended.
    #[cfg(target_os = "macos")]
    const PNG: &[u8] = include_bytes!("../../../assets/icon/oneship-designer-256.png");
    #[cfg(not(target_os = "macos"))]
    const PNG: &[u8] = include_bytes!("../../../assets/icon/oneship-designer-256.png");
    let img = image::load_from_memory_with_format(PNG, image::ImageFormat::Png).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData { rgba: img.into_raw(), width, height })
}

/// A windowed process has no console of its own. If it was started from a terminal, attach to
/// that one so `--help`, `--version` and errors still print there. Rust reads the console handles
/// on the first print, so this has to run before any output.
#[cfg(all(windows, not(debug_assertions), not(test)))]
fn attach_parent_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    unsafe {
        AttachConsole(u32::MAX);
    }
}

fn kind_label(kind: &desigma_doc::NodeKind) -> String {
    let raw = format!("{kind:?}");
    raw.split(['(', '{', ' ']).next().unwrap_or("Node").to_string()
}

fn collect_layers(
    doc: &desigma_doc::Document,
    node_id: desigma_doc::NodeId,
    depth: usize,
    out: &mut Vec<serde_json::Value>,
) {
    if out.len() >= 400 || depth > 5 {
        return;
    }
    let Some(node) = doc.get(node_id) else {
        return;
    };
    out.push(serde_json::json!({
        "id": node.id.0,
        "name": node.name,
        "kind": kind_label(&node.kind),
        "depth": depth,
        "has_children": !node.children.is_empty(),
    }));
    for child in &node.children {
        collect_layers(doc, *child, depth + 1, out);
    }
}

fn run_preview_json(file: &std::path::Path, requested_node: Option<u64>) -> Result<String, String> {
    let doc = io::load_path(file)?;
    let pages: Vec<serde_json::Value> = doc
        .pages
        .iter()
        .filter_map(|pid| doc.get(*pid))
        .map(|p| serde_json::json!({ "id": p.id.0, "name": p.name }))
        .collect();

    let default_page = doc.pages.first().copied().unwrap_or(desigma_doc::NodeId(0));
    let target_id = requested_node
        .map(desigma_doc::NodeId)
        .filter(|id| doc.get(*id).is_some())
        .unwrap_or(default_page);

    // Find which page contains target_id so the layer tree always shows that page's hierarchy.
    let active_page_id = if doc.pages.contains(&target_id) {
        target_id
    } else {
        doc.pages
            .iter()
            .copied()
            .find(|pid| doc.is_ancestor(*pid, target_id))
            .unwrap_or(default_page)
    };

    let mut layers = Vec::new();
    if let Some(page_node) = doc.get(active_page_id) {
        for child in &page_node.children {
            collect_layers(&doc, *child, 0, &mut layers);
        }
    }

    let font_report = desigma_fonts::ensure_fonts(&doc, &desigma_fonts::FontCache::new());
    for loaded in font_report.loaded {
        desigma_export::register_font(loaded.data);
    }

    let svg = desigma_export::export_svg(&doc, target_id, &desigma_export::SvgOptions::default());
    let payload = serde_json::json!({
        "name": doc.name,
        "active_page_id": active_page_id.0,
        "selected_node_id": target_id.0,
        "pages": pages,
        "layers": layers,
        "svg": svg,
    });
    serde_json::to_string(&payload).map_err(|e| format!("Failed to serialize preview JSON: {e}"))
}

fn main() {
    crash::install();
    #[cfg(all(windows, not(debug_assertions), not(test)))]
    attach_parent_console();
    let opts = match cli::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("desigma: {e}\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if opts.help {
        println!("{}", cli::USAGE);
        return;
    }
    if opts.version {
        println!("desigma {VERSION}");
        return;
    }
    if opts.print_library_path {
        let root = opts.library.clone().unwrap_or_else(desigma_library::Library::default_root);
        println!("{}", root.display());
        return;
    }
    if let Some(file) = &opts.export_animation {
        match motion::playback::export::run_cli(file, opts.format.as_deref(), opts.out.as_deref(), opts.fps, opts.scale, opts.select.as_deref(), opts.dither) {
            Ok(msg) => println!("desigma: {msg}"),
            Err(e) => {
                eprintln!("desigma: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(dir) = &opts.seed_library {
        match showcase::seed_library(dir) {
            Ok(()) => println!("desigma: wrote the showcase sample files to {}", dir.display()),
            Err(e) => {
                eprintln!("desigma: cannot seed {}: {e}", dir.display());
                std::process::exit(1);
            }
        }
        return;
    }
    if opts.preview_json {
        let Some(file) = &opts.file else {
            eprintln!("desigma: --preview-json requires a file path");
            std::process::exit(2);
        };
        match run_preview_json(file, opts.node_id) {
            Ok(json_out) => println!("{json_out}"),
            Err(e) => {
                eprintln!("desigma: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // A plain `desigma FILE|desigma://join/LINK` (what the OS runs for a click) goes to the editor
    // that is already running, without opening a window here.
    let only_open = cli::Options { file: opts.file.clone(), join: opts.join.clone(), ..Default::default() } == opts;
    if only_open && (opts.file.is_some() || opts.join.is_some()) {
        let dir = instance::default_dir();
        let mut opens = opts.join.iter().cloned().map(instance::Open::Link).chain(opts.file.iter().cloned().map(instance::Open::File));
        if opens.all(|o| instance::forward(&dir, &o)) {
            return;
        }
    }
    #[cfg(target_os = "macos")]
    macos::register();
    let (w, h) = opts.size.unwrap_or((theme::DEFAULT_WINDOW.0 as u32, theme::DEFAULT_WINDOW.1 as u32));
    let screenshot_mode = opts.screenshot.is_some();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(theme::display_title())
        .with_inner_size([w as f32, h as f32])
        .with_min_inner_size([theme::MIN_WINDOW.0, theme::MIN_WINDOW.1]);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    if screenshot_mode {
        viewport = viewport.with_min_inner_size([200.0, 200.0]).with_resizable(true);
    }
    let native = eframe::NativeOptions { viewport, ..Default::default() };
    let result = eframe::run_native(theme::APP_TITLE, native, Box::new(move |cc| Ok(Box::new(shell::Shell::new(cc, opts)))));
    if let Err(e) = result {
        eprintln!("desigma: {e}");
        std::process::exit(1);
    }
    // normally started when the window closed; this catches a close that skipped that frame
    updater::install::run_stashed();
    if screenshot_mode && !shell::SHOT_OK.load(Ordering::SeqCst) {
        let why = shell::SHOT_ERR.lock().map(|e| e.clone()).unwrap_or_default();
        let why = if why.is_empty() { "the window closed before a frame was captured".to_string() } else { why };
        eprintln!("desigma: screenshot was not captured: {why}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod icon_tests {
    #[test]
    fn embedded_window_icon_decodes() {
        let icon = super::window_icon().expect("icon decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }
}
