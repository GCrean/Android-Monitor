// GUI subsystem on Windows: no console window when launched from Explorer / Start menu.
// (Terminal use is restored by `attach_parent_console` below.)
#![cfg_attr(windows, windows_subsystem = "windows")]

mod adb;
mod charts;
mod export;
mod mcp;
mod telemetry;
mod theme;
mod ui;

use futures::StreamExt;
use gpui::{prelude::*, px, size, App, Application, Bounds, WindowBounds, WindowOptions};

use telemetry::Hub;

const HELP: &str = "\
Android Monitor - live Android telemetry over adb

USAGE: android-monitor [OPTIONS]

  --mcp-stdio          Run headless as an MCP server on stdin/stdout (for MCP clients
                       that spawn the process). Add --gui to also open the window.
  --mcp-tcp [ADDR]     Serve MCP over TCP (newline-delimited JSON-RPC), default
                       127.0.0.1:8765. Works alongside the GUI, or with --headless.
  --mcp-readonly       Refuse MCP tools that change the device (install, uninstall,
                       launch, stop, grant/revoke permissions).
  --headless           Do not open the window (implied by --mcp-stdio).
  --gui                Open the window even with --mcp-stdio.
  -h, --help           Show this help
";

struct Args {
    stdio: bool,
    tcp: Option<String>,
    headless: bool,
}

fn parse_args() -> Args {
    let mut a = Args { stdio: false, tcp: None, headless: false };
    let mut gui = false;
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--mcp-stdio" => a.stdio = true,
            "--mcp-tcp" => {
                let addr = match it.peek() {
                    Some(n) if !n.starts_with('-') => it.next().unwrap(),
                    _ => "127.0.0.1:8765".to_string(),
                };
                a.tcp = Some(addr);
            }
            "--headless" => a.headless = true,
            "--mcp-readonly" => mcp::READ_ONLY.store(true, std::sync::atomic::Ordering::Relaxed),
            "--gui" => gui = true,
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown option: {other}\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }
    if a.stdio && !gui {
        a.headless = true;
    }
    a
}

fn run_headless(hub: Hub, stdio: bool) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    telemetry::spawn_worker(tx, hub.sel.clone());
    let shared = hub.tel.clone();
    std::thread::spawn(move || {
        futures::executor::block_on(async move {
            while let Some(m) = rx.next().await {
                shared.lock().unwrap_or_else(|e| e.into_inner()).apply(m);
            }
        })
    });
    if stdio {
        mcp::serve_stdio(&hub);
    } else {
        loop {
            std::thread::park();
        }
    }
}

/// A GUI-subsystem exe starts with no console. When launched from a terminal, attach to it and
/// point any std handle that isn't already redirected at it, so `--help` and log lines still print.
/// Redirected pipes (e.g. an MCP client using --mcp-stdio) are left untouched.
#[cfg(windows)]
fn attach_parent_console() {
    use std::fs::OpenOptions;
    use std::os::windows::io::IntoRawHandle;
    type Handle = *mut core::ffi::c_void;
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn GetStdHandle(id: u32) -> Handle;
        fn SetStdHandle(id: u32, h: Handle) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_INPUT: u32 = -10i32 as u32;
    const STD_OUTPUT: u32 = -11i32 as u32;
    const STD_ERROR: u32 = -12i32 as u32;
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return; // no parent console (double-click, service, ...)
        }
        for (id, name, write) in [(STD_INPUT, "CONIN$", false), (STD_OUTPUT, "CONOUT$", true), (STD_ERROR, "CONOUT$", true)] {
            let h = GetStdHandle(id);
            if h.is_null() || h as isize == -1 {
                if let Ok(f) = OpenOptions::new().read(!write).write(write).open(name) {
                    SetStdHandle(id, f.into_raw_handle() as Handle);
                }
            }
        }
    }
}

fn main() {
    #[cfg(windows)]
    attach_parent_console();
    let args = parse_args();
    let hub = Hub::new();

    if let Some(addr) = &args.tcp {
        match mcp::serve_tcp(hub.clone(), addr) {
            Ok(a) => {
                eprintln!("MCP TCP server listening on {a}");
                if !a.ip().is_loopback() {
                    eprintln!("warning: listening on a non-loopback address; the server has no authentication");
                }
            }
            Err(e) => {
                eprintln!("cannot bind MCP TCP server on {addr}: {e}");
                std::process::exit(1);
            }
        }
    }

    if args.headless {
        if !args.stdio && args.tcp.is_none() {
            eprintln!("--headless needs --mcp-stdio or --mcp-tcp");
            std::process::exit(2);
        }
        run_headless(hub, args.stdio);
        return;
    }

    if args.stdio {
        // --mcp-stdio --gui: serve stdio on a thread while the window runs.
        let h = hub.clone();
        std::thread::spawn(move || mcp::serve_stdio(&h));
    }

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Android Monitor".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| ui::Monitor::new(hub, cx)),
        )
        .expect("open window");
        cx.activate(true);
    });
}
