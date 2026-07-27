mod config;
mod document;
mod editor;
mod keymap;
mod markdown;
mod perf;
mod session;
mod tasks;
mod theme;
mod vault;
mod vim;
mod watcher;

use std::borrow::Cow;
use std::path::PathBuf;

use config::Config;
use editor::Editor;
use gpui::{
    App, Application, AssetSource, Bounds, Focusable, SharedString, WindowBounds, WindowOptions,
    prelude::*, px, size,
};

/// Sidebar icons (lucide, ISC — `assets/icons/LICENSE-lucide.txt`), compiled
/// in like the fonts. gpui's `svg()` element loads by path through the app's
/// registered `AssetSource`.
// ponytail: five hardcoded icons — bring in a directory-embedding dep only if
// the set actually grows.
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        macro_rules! icon {
            ($name:literal) => {
                Some(include_bytes!(concat!("../assets/icons/", $name)).as_slice().into())
            };
        }
        Ok(match path {
            "icons/chevron-right.svg" => icon!("chevron-right.svg"),
            "icons/chevron-down.svg" => icon!("chevron-down.svg"),
            "icons/folder.svg" => icon!("folder.svg"),
            "icons/file-text.svg" => icon!("file-text.svg"),
            "icons/file-code.svg" => icon!("file-code.svg"),
            "icons/trash.svg" => icon!("trash.svg"),
            "icons/check.svg" => icon!("check.svg"),
            _ => None,
        })
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

fn main() {
    perf::launch();

    // `-d`/`--detach`: re-spawn ourselves without the flag — stdio on /dev/null,
    // own process group (Unix) / detached from the console (Windows) — and exit,
    // returning the shell prompt while the window lives on.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-d" || a == "--detach") {
        use std::process::{Command, Stdio};
        let exe = std::env::current_exe().expect("current_exe");
        let mut cmd = Command::new(exe);
        cmd.args(args.iter().filter(|a| *a != "-d" && *a != "--detach"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0); // outside the tty's foreground group, so terminal-close SIGHUP misses it
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0000_0008); // DETACHED_PROCESS
        }
        cmd.spawn().expect("spawn detached darknotes");
        return;
    }

    // WSLg advertises a Wayland compositor whose version GPUI rejects; force the
    // X11 backend (DISPLAY) there by hiding WAYLAND_DISPLAY before GPUI probes it.
    // ponytail: WSL-only guard; native Wayland desktops keep their compositor.
    if std::env::var_os("WSL_DISTRO_NAME").is_some() {
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
    }

    Application::new().with_assets(Assets).run(|cx: &mut App| {
        // Embedded so both fonts render identically on every machine, with no
        // system-install step. Courier Prime (editor) ships all four styles
        // because markdown rendering uses bold/italic runs; Inter (UI chrome)
        // ships only the styles the chrome uses — regular everywhere, italic
        // for preview tabs. A missing style would fall back to another family.
        cx.text_system()
            .add_fonts(vec![
                include_bytes!("../assets/fonts/CourierPrime-Regular.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../assets/fonts/CourierPrime-Bold.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../assets/fonts/CourierPrime-Italic.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../assets/fonts/CourierPrime-BoldItalic.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../assets/fonts/Inter-Regular.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../assets/fonts/Inter-Italic.ttf")
                    .as_slice()
                    .into(),
            ])
            .expect("embedded fonts are valid TTFs");

        let config = Config::load();
        let theme = theme::Theme::by_name(&config.theme).unwrap_or_else(|| {
            eprintln!("darknotes: unknown theme {:?}; using default", config.theme);
            theme::Theme::default()
        });
        cx.set_global(theme);

        // A directory argument opens a vault; a file argument opens that file
        // with its parent as the vault. With no argument, fall back to the
        // configured vault, then the current directory.
        let (root, initial) = match std::env::args().nth(1) {
            Some(p) => {
                let path = PathBuf::from(p);
                if path.is_dir() {
                    (path, None)
                } else {
                    let root = path
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from("."));
                    (root, Some(path))
                }
            }
            None => {
                let root = config
                    .vault_path()
                    .filter(|p| p.is_dir())
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("."));
                (root, None)
            }
        };

        // Start maximized; the centered size is the restore bounds.
        let bounds = Bounds::centered(None, size(px(900.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Maximized(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                let editor = cx.new(|cx| Editor::new(root, initial, config, window, cx));
                // Focus on launch, or on_key_down never fires and nothing types.
                let handle = editor.read(cx).focus_handle(cx);
                window.focus(&handle);
                editor
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
